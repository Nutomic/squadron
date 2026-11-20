use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use llm_gateway::{lane_model, MachineLlmClient};
use prompts::{
    gate_with_options, parse, DiversityOutput, GateOptions, ImplementerOutput, JudgeOutput, Role,
    ValidatorOutput, Verdict,
};
use serde_json::Value;
use sprites_client::{ExecOptions, SpriteClient};
use store::{Run, Store};

use crate::artifacts;
use crate::config::{ms_after, now_ms, PipelineConfig};
use crate::harness::{self, HarnessCache};
use crate::judge;
use crate::lane::LaneState;
use crate::screenshot::ScreenshotQueue;
use crate::slots::SlotSemaphore;
use crate::snapshot;
use crate::sse::SseHub;

#[derive(Debug, Clone)]
pub struct PipelineCtx {
    pub store: Store,
    pub sprite: SpriteClient,
    pub machine_llm: Option<Arc<MachineLlmClient>>,
    pub sse: SseHub,
    pub screenshots: ScreenshotQueue,
    pub config: PipelineConfig,
    pub slots: SlotSemaphore,
    /// Probed-once harness chain winner, cached for the process lifetime.
    pub harness_cache: Arc<HarnessCache>,
    pub hub_registry:
        Option<std::sync::Arc<tokio::sync::RwLock<std::collections::HashMap<String, SseHub>>>>,
}

/// How to obtain implementer/validator outputs for a lane.
///
/// `Real` runs the actual harness chain inside the sprite and is the only mode
/// used by the public API. `Command` is TEST-ONLY: it execs a fixed command that
/// prints JSON to stdout, and must never be reachable from untrusted input.
#[derive(Debug, Clone)]
pub enum HarnessMode {
    /// Real mode: attempts the opencode -> codex -> claude -> curl-loop chain.
    Real { gateway_connector_id: String },
    /// Test-only hook: exec `command` inside sprite; stdout must be schema-valid
    /// JSON for the role. Never constructed from API/web input.
    Command {
        impl_command: String,
        val_command: String,
    },
}

impl Default for HarnessMode {
    fn default() -> Self {
        HarnessMode::Real {
            gateway_connector_id: "sanCSEiLn9Np-G_Y5q2jKA".to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub brief: String,
    pub n_lanes: usize,
    pub harness: HarnessMode,
}

pub async fn start_run(ctx: Arc<PipelineCtx>, req: RunRequest) -> Result<Run, String> {
    let n = req.n_lanes;
    PipelineConfig::validate_n(n).map_err(|e| e)?;
    let created = now_ms();
    let run = ctx
        .store
        .create_run(&req.brief, n as i64, created)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(reg) = &ctx.hub_registry {
        let mut hubs = reg.write().await;
        hubs.insert(run.id.clone(), ctx.sse.clone());
    }

    // diversity
    let variants = generate_variants(&ctx, &req.brief, n).await;
    for (idx, v) in variants.iter().enumerate() {
        let vjson = serde_json::to_string(v).unwrap_or_else(|_| "{}".to_owned());
        ctx.store
            .insert_lane(&run.id, idx as i64, &vjson)
            .await
            .map_err(|e| e.to_string())?;
    }
    ctx.store
        .update_run_status(&run.id, "running")
        .await
        .map_err(|e| e.to_string())?;
    publish(&ctx, &run.id).await;

    let run_id = run.id.clone();
    let ctx_bg = ctx.clone();
    tokio::spawn(async move {
        if let Err(e) = execute_run(ctx_bg.clone(), run_id.clone(), req.harness).await {
            let _ = ctx_bg.store.update_run_status(&run_id, "failed").await;
            let ts = now_ms();
            for i in 0..n as i64 {
                let _ = ctx_bg
                    .store
                    .append_lane_event(&run_id, i, ts, "log", &format!("run_failed: {e}"))
                    .await;
            }
            publish(&ctx_bg, &run_id).await;
        }
    });

    ctx.store
        .get_run(&run.id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "run missing".to_owned())
}

async fn generate_variants(ctx: &PipelineCtx, brief: &str, n: usize) -> Vec<Value> {
    // try machine LLM diversity generator if available
    if let Some(llm) = &ctx.machine_llm {
        let prompt = prompts::diversity_prompt(brief, n);
        let request = llm_gateway::ChatRequest::new(llm_gateway::DIVERSITY_MODEL)
            .system(&prompt.system)
            .user(&prompt.user);
        if let Ok(resp) = llm.chat(&request).await {
            if let Some(text) = resp.text() {
                let opts = GateOptions {
                    expected_variants: Some(n),
                    max_verdict_sentences: None,
                    expected_lane_indices: None,
                };
                if gate_with_options(Role::Diversity, &text, &opts).is_ok() {
                    if let Ok(parsed) = parse::<DiversityOutput>(Role::Diversity, &text) {
                        return parsed
                            .variants
                            .into_iter()
                            .map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
                            .collect();
                    }
                }
            }
        }
    }
    // fallback
    prompts::template_variants(n)
        .into_iter()
        .map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
        .collect()
}

async fn execute_run(
    ctx: Arc<PipelineCtx>,
    run_id: String,
    harness: HarnessMode,
) -> Result<(), String> {
    let lanes = ctx
        .store
        .list_lanes(&run_id)
        .await
        .map_err(|e| e.to_string())?;
    let _n = lanes.len();

    // Run lanes concurrently, bounded by slot semaphore inside lane execution
    let mut handles = Vec::new();
    for lane in lanes {
        let ctx2 = ctx.clone();
        let run_id2 = run_id.clone();
        let harness2 = harness.clone();
        handles.push(tokio::spawn(async move {
            run_lane(ctx2, run_id2, lane.idx, harness2).await
        }));
    }
    for h in handles {
        let _ = h.await;
    }

    // Judge if >=2 succeeded (validated lanes)
    let lanes = ctx
        .store
        .list_lanes(&run_id)
        .await
        .map_err(|e| e.to_string())?;
    let validated: Vec<_> = lanes
        .iter()
        .filter(|l| l.state == "validated")
        .cloned()
        .collect();
    if validated.len() >= 2 {
        run_judge_phase(&ctx, &run_id, &validated).await?;
    } else {
        // mark run failed if not enough
        let _ = ctx.store.update_run_status(&run_id, "failed").await;
    }

    publish(&ctx, &run_id).await;
    Ok(())
}

async fn run_lane(
    ctx: Arc<PipelineCtx>,
    run_id: String,
    idx: i64,
    harness: HarnessMode,
) -> Result<(), String> {
    let mut attempts = 0;
    let max_attempts = ctx.config.retries_per_lane + 1;
    loop {
        attempts += 1;
        let res = run_lane_attempt(ctx.clone(), run_id.clone(), idx, &harness).await;
        match res {
            Ok(()) => return Ok(()),
            Err(e) => {
                let transient = is_transient_error(&e);
                let ts = now_ms();
                let _ = ctx
                    .store
                    .append_lane_event(
                        &run_id,
                        idx,
                        ts,
                        "log",
                        &format!("lane attempt {attempts} failed: {e}"),
                    )
                    .await;
                if transient && attempts < max_attempts {
                    // #8: a retry cannot succeed while the previous sprite
                    // still exists (deterministic name, real `sprite create`
                    // rejects duplicates). Destroy --force it and reset the
                    // lane so the next attempt starts clean.
                    if let Ok(Some(l)) = ctx.store.get_lane(&run_id, idx).await {
                        if let Some(name) = l.sprite_name {
                            let _ = ctx.sprite.destroy(&name).await;
                        }
                    }
                    let _ = ctx.store.set_lane_error(&run_id, idx, "").await;
                    // The harness choice may have been an install failure;
                    // invalidate so the next attempt re-probes the chain.
                    ctx.harness_cache.invalidate().await;
                    continue;
                } else {
                    // hard fail / skip
                    let _ = ctx
                        .store
                        .set_lane_error(&run_id, idx, &lane_error_message(&e))
                        .await;
                    let _ = transition(&ctx, &run_id, idx, LaneState::Failed).await;
                    // best-effort destroy
                    if let Ok(Some(l)) = ctx.store.get_lane(&run_id, idx).await {
                        if let Some(name) = l.sprite_name {
                            let _ = ctx.sprite.destroy(&name).await;
                        }
                    }
                    publish(&ctx, &run_id).await;
                    return Err(e);
                }
            }
        }
    }
}

fn is_transient_error(e: &str) -> bool {
    let l = e.to_lowercase();
    l.contains("timeout")
        || l.contains("temporar")
        || l.contains("429")
        || l.contains("503")
        || l.contains("connection")
        || l.contains("spawn")
}

async fn run_lane_attempt(
    ctx: Arc<PipelineCtx>,
    run_id: String,
    idx: i64,
    harness: &HarnessMode,
) -> Result<(), String> {
    // --- Implementer phase: acquire slot ---
    let permit = ctx.slots.acquire().await;
    transition(&ctx, &run_id, idx, LaneState::Creating).await?;

    let lane = ctx
        .store
        .get_lane(&run_id, idx)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("lane {idx} missing"))?;
    let variant_json = lane.variant_json.clone();

    let short = short_run_id(&run_id);
    let sprite_name = format!("swarm-{short}-impl-{idx}");
    // create sprite; if a stale sprite still holds the deterministic name,
    // destroy --force it once and retry the create.
    if let Err(e) = ctx.sprite.create(&sprite_name).await {
        let _ = ctx
            .store
            .append_lane_event(
                &run_id,
                idx,
                now_ms(),
                "log",
                &format!("create failed ({e}); destroying stale sprite and retrying"),
            )
            .await;
        let _ = ctx.sprite.destroy(&sprite_name).await;
        ctx.sprite
            .create(&sprite_name)
            .await
            .map_err(|e| e.to_string())?;
    }
    let _ = ctx.sprite.set_url_auth_public().await; // best effort; may fail in mock - mock handles it
    let impl_url = format!("https://{sprite_name}.sprites.app");

    let started = now_ms();
    let deadline = ms_after(started, ctx.config.impl_deadline);
    let model = lane_model(idx as usize).to_owned();
    ctx.store
        .start_impl(
            &run_id,
            idx,
            &sprite_name,
            &impl_url,
            &model,
            started,
            deadline,
        )
        .await
        .map_err(|e| e.to_string())?;
    transition(&ctx, &run_id, idx, LaneState::Running).await?;
    publish(&ctx, &run_id).await;

    // run implementer harness (deadline enforced)
    let impl_result = tokio::time::timeout(
        ctx.config.impl_deadline,
        run_implementer(
            &ctx,
            &run_id,
            idx,
            &sprite_name,
            &model,
            &variant_json,
            harness,
        ),
    )
    .await;
    // slot freed after implementer session ends (sprite warm)
    drop(permit);
    let _ = ctx
        .store
        .append_lane_event(&run_id, idx, now_ms(), "log", "impl_session_end_slot_freed")
        .await;

    let impl_out = match impl_result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(format!("implementer: {e}")),
        Err(_) => return Err("implementer: deadline exceeded".to_owned()),
    };
    ctx.store
        .finish_impl(&run_id, idx, &impl_out.summary, impl_out.files as i64)
        .await
        .map_err(|e| e.to_string())?;
    ctx.store
        .add_lane_cost(&run_id, idx, impl_out.cost_usd)
        .await
        .ok();
    let _ = ctx
        .store
        .append_lane_event(
            &run_id,
            idx,
            now_ms(),
            "log",
            &format!("impl run_command: {}", impl_out.run_command),
        )
        .await;

    // #10: the sprite URL must answer with 200 before the lane can be Done.
    // Test mode uses fake URLs, so the probe is skipped to stay deterministic;
    // the 127.0.0.1 self-probe fallback is gone (a passing orchestrator is not
    // a passing lane).
    if !ctx.config.test_mode && !artifacts::check_http_200(&impl_url).await {
        return Err(format!(
            "implementer: live URL did not answer 200: {impl_url}"
        ));
    }
    transition(&ctx, &run_id, idx, LaneState::Done).await?;
    publish(&ctx, &run_id).await;

    // artifacts at lane completion (after impl, before validation).
    // Real mode pulls them FROM the sprite (`sprite exec` tar + tokei);
    // test mode collects the local lane workdir to stay deterministic.
    let artifacts_dir = artifacts::lane_artifacts_dir(&ctx.config, &run_id, idx);
    let console_tail = impl_out.console_tail.clone();
    let collected = if ctx.config.test_mode {
        let workdir = lane_workdir(&ctx, &run_id, idx);
        artifacts::collect_artifacts(&ctx.config, &run_id, idx, &workdir, &console_tail).await
    } else {
        artifacts::collect_artifacts_from_sprite(
            &ctx.sprite,
            &sprite_name,
            &sprite_workdir(&run_id, idx),
            &artifacts_dir,
            &console_tail,
        )
        .await
    };
    if let Ok((tarball, tokei_json, files_n, _loc_n)) = collected {
        let files_store = files_n.or(Some(impl_out.files as i64));
        let _ = ctx
            .store
            .set_artifacts(
                &run_id,
                idx,
                &tarball,
                &tokei_json,
                None,
                "pending",
                &console_tail,
            )
            .await;
        if let Some(f) = files_store {
            let _ = ctx
                .store
                .finish_impl(&run_id, idx, &impl_out.summary, f)
                .await;
        }
    } else {
        // minimal artifacts record
        let tokei_json = serde_json::json!({"loc": 0, "files": impl_out.files}).to_string();
        let _ = ctx
            .store
            .set_artifacts(
                &run_id,
                idx,
                "",
                &tokei_json,
                None,
                "pending",
                &console_tail,
            )
            .await;
    }

    // #28: checkpoint the sprite after the implementer is done, before
    // validation, so validation/retries can restart from a known state.
    let _ = ctx.sprite.checkpoint(&sprite_name).await;

    // --- Validator phase: acquires its own slot ---
    let _val_permit = ctx.slots.acquire().await;
    let val_started = now_ms();
    let val_deadline = ms_after(val_started, ctx.config.val_deadline);
    let val_model = lane_model(idx as usize).to_owned(); // alternate same scheme; spec states validators alternate per lane too
    ctx.store
        .start_validation(&run_id, idx, &val_model, val_started, val_deadline)
        .await
        .map_err(|e| e.to_string())?;
    let _ = ctx
        .store
        .append_lane_event(&run_id, idx, val_started, "state", "validating")
        .await;
    publish(&ctx, &run_id).await;

    let val_result = tokio::time::timeout(
        ctx.config.val_deadline,
        run_validator(
            &ctx,
            &run_id,
            idx,
            &sprite_name,
            &impl_url,
            &impl_out.summary,
            harness,
        ),
    )
    .await;
    drop(_val_permit);

    let val_out = match val_result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(format!("validator: {e}")),
        Err(_) => return Err("validator: deadline exceeded".to_owned()),
    };
    let verdict_str = match val_out.verdict {
        Verdict::Pass => "pass",
        Verdict::Fail => "fail",
    };
    ctx.store
        .finish_validation(
            &run_id,
            idx,
            verdict_str,
            &val_out.summary,
            val_out.quality_score,
        )
        .await
        .map_err(|e| e.to_string())?;
    ctx.store
        .add_lane_cost(&run_id, idx, val_out.cost_usd)
        .await
        .ok();
    transition(&ctx, &run_id, idx, LaneState::Validated).await?;
    publish(&ctx, &run_id).await;

    Ok(())
}

#[derive(Debug, Clone)]
struct ImplRunResult {
    summary: String,
    files: u32,
    run_command: String,
    cost_usd: f64,
    console_tail: String,
}

#[derive(Debug, Clone)]
struct ValRunResult {
    verdict: Verdict,
    summary: String,
    quality_score: f64,
    cost_usd: f64,
}

async fn run_implementer(
    ctx: &PipelineCtx,
    run_id: &str,
    idx: i64,
    sprite_name: &str,
    model: &str,
    variant_json: &str,
    harness: &HarnessMode,
) -> Result<ImplRunResult, String> {
    let stdout = match harness {
        HarnessMode::Command { impl_command, .. } => {
            let workdir = lane_workdir(ctx, run_id, idx);
            let _ = std::fs::create_dir_all(&workdir);
            let opts = ExecOptions::default()
                .with_dir(&workdir)
                .with_timeout(ctx.config.impl_deadline);
            let out = ctx
                .sprite
                .exec(sprite_name, impl_command, &opts)
                .await
                .map_err(|e| format!("implementer exec: {e}"))?;
            out.stdout
        }
        HarnessMode::Real { .. } => {
            let variant: Value = serde_json::from_str(variant_json)
                .map_err(|e| format!("implementer variant: {e}"))?;
            let prompt = prompts::implementer_prompt(&variant);
            let recipe =
                llm_gateway::SpriteRecipe::for_model(format!("{run_id}-{idx}"), model.to_owned());
            let connector_env = (
                "OPENCODE_GATEWAY_CONNECTOR".to_owned(),
                ctx.config.gateway_connector_id.clone(),
            );
            let run = harness::run_prompt(
                &ctx.harness_cache,
                &ctx.sprite,
                sprite_name,
                &prompt.system,
                &prompt.user,
                &recipe,
                ctx.config.impl_deadline,
                Some(connector_env),
                Some(&sprite_workdir(run_id, idx)),
            )
            .await
            .map_err(|e| format!("implementer: {e}"))?;
            let _ = ctx
                .store
                .append_lane_event(
                    run_id,
                    idx,
                    now_ms(),
                    "log",
                    &format!("harness choice: {}", run.choice.as_str()),
                )
                .await;
            run.output
        }
    };
    let stdout = stdout.trim().to_string();
    let parsed: ImplementerOutput =
        gate_last_json(Role::Implementer, &stdout, &GateOptions::default())
            .map_err(|e| format!("implementer: schema gate failed ({e})"))?;
    // #13: honor the implementer's own report; a not-done or blocked claim is
    // a lane failure before artifacts/validation.
    if !parsed.done {
        return Err("implementer: reported done=false".to_owned());
    }
    if !parsed.blockers.is_empty() {
        return Err(format!(
            "implementer: reported blockers ({})",
            parsed.blockers.join("; ")
        ));
    }
    let _ = gate_injection_free(Role::Implementer, &run_id, idx, &stdout, ctx).await;
    Ok(ImplRunResult {
        summary: parsed.summary,
        files: parsed.files,
        run_command: parsed.run_command,
        cost_usd: cost_from_output(&stdout).unwrap_or(0.0),
        console_tail: tail_4k(&stdout),
    })
}

async fn run_validator(
    ctx: &PipelineCtx,
    run_id: &str,
    idx: i64,
    sprite_name: &str,
    impl_url: &str,
    impl_summary: &str,
    harness: &HarnessMode,
) -> Result<ValRunResult, String> {
    let stdout = match harness {
        HarnessMode::Command { val_command, .. } => {
            let workdir = lane_workdir(ctx, run_id, idx);
            let _ = std::fs::create_dir_all(&workdir);
            let opts = ExecOptions::default()
                .with_dir(&workdir)
                .with_timeout(ctx.config.val_deadline);
            let out = ctx
                .sprite
                .exec(sprite_name, val_command, &opts)
                .await
                .map_err(|e| format!("validator exec: {e}"))?;
            out.stdout
        }
        HarnessMode::Real { .. } => {
            // Real mode: run the real validator report prompt through the
            // harness against the actual code in the lane workdir and the
            // live site. Never fabricate a verdict.
            let workdir = sprite_workdir(run_id, idx);
            let prompt = prompts::validator_prompt(impl_summary, impl_url, &workdir);
            let recipe = llm_gateway::SpriteRecipe::for_model(
                format!("{run_id}-{idx}-val"),
                lane_model(idx as usize).to_owned(),
            );
            let connector_env = (
                "OPENCODE_GATEWAY_CONNECTOR".to_owned(),
                ctx.config.gateway_connector_id.clone(),
            );
            let run = harness::run_prompt(
                &ctx.harness_cache,
                &ctx.sprite,
                sprite_name,
                &prompt.system,
                &prompt.user,
                &recipe,
                ctx.config.val_deadline,
                Some(connector_env),
                Some(&sprite_workdir(run_id, idx)),
            )
            .await
            .map_err(|e| format!("validator: {e}"))?;
            run.output
        }
    };
    let stdout = stdout.trim().to_string();
    let parsed: ValidatorOutput = gate_last_json(Role::Validator, &stdout, &GateOptions::default())
        .map_err(|e| format!("validator: schema gate failed ({e})"))?;
    let _ = gate_injection_free(Role::Validator, &run_id, idx, &stdout, ctx).await;
    // Persist the full report (features/bugs/security) so the judge and the
    // lane detail view can be hydrated from the store.
    if let Ok(report_json) = serde_json::to_string(&parsed) {
        let _ = ctx
            .store
            .set_validator_report(&run_id, idx, &report_json)
            .await;
    }
    Ok(ValRunResult {
        verdict: parsed.verdict,
        summary: parsed.summary,
        quality_score: parsed.quality_score,
        cost_usd: cost_from_output(&stdout).unwrap_or(0.0),
    })
}

/// #12: load each validated lane's rendered screenshot (if any) and return it
/// as a data-URL image part, aligned with the report order. Missing renders map
/// to None so the judge degrades to text-only for that lane.
async fn collect_lane_screenshot_images(
    run_id: &str,
    lanes: &[store::Lane],
) -> Vec<Option<String>> {
    let mut out = Vec::with_capacity(lanes.len());
    for lane in lanes {
        let path = lane.screenshot_path.clone().unwrap_or_else(|| {
            crate::screenshot::render_dest(run_id, lane.idx)
                .to_string_lossy()
                .into_owned()
        });
        let data = tokio::fs::read(&path).await.ok();
        out.push(data.map(|bytes| {
            let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
            format!("data:image/png;base64,{b64}")
        }));
    }
    out
}

async fn run_judge_phase(
    ctx: &PipelineCtx,
    run_id: &str,
    validated: &[store::Lane],
) -> Result<(), String> {
    // gather validator reports and sanitized extracts
    let reports = judge::validator_reports_from_lanes(validated);
    let mut extracts = Vec::new();
    for l in validated {
        if ctx.config.test_mode {
            extracts.push(format!("<test extract for lane {}>", l.idx));
        } else if let Some(url) = &l.impl_url {
            let ext = judge::fetch_sanitized_extract(url).await;
            if ext.is_empty() {
                extracts.push(format!("<empty extract for lane {}>", l.idx));
            } else {
                extracts.push(ext);
            }
        } else {
            extracts.push(String::new());
        }
    }
    let (verdict, cost) = if let Some(llm) = &ctx.machine_llm {
        let images = collect_lane_screenshot_images(run_id, validated).await;
        judge::run_judge(llm, &reports, &extracts, &images, ctx.config.judge_deadline).await?
    } else {
        // fallback deterministic judge (should not happen in proper runs; but keep safe)
        let winner = validated.first().map(|l| l.idx as usize).unwrap_or(0);
        (
            JudgeOutput {
                ranking: validated.iter().map(|l| l.idx as usize).collect(),
                winner,
                verdict_card: "Winner selected by fallback. Criteria met.".to_owned(),
                reasons: prompts::TextOrList::Text("fallback".to_owned()),
            },
            0.0,
        )
    };
    let ts = now_ms();
    // compute total cost from lanes + judge
    let lanes = ctx
        .store
        .list_lanes(run_id)
        .await
        .map_err(|e| e.to_string())?;
    let total_cost: f64 = lanes.iter().filter_map(|l| l.cost_usd).sum::<f64>() + cost;
    ctx.store
        .finish_run(
            run_id,
            "judged",
            &verdict.verdict_card,
            verdict.winner as i64,
            total_cost,
            ts,
        )
        .await
        .map_err(|e| e.to_string())?;
    for l in &lanes {
        let _ = ctx
            .store
            .append_lane_event(run_id, l.idx, ts, "log", "run_judged")
            .await;
    }
    Ok(())
}

async fn transition(
    ctx: &PipelineCtx,
    run_id: &str,
    idx: i64,
    next: LaneState,
) -> Result<(), String> {
    // best-effort state validation based on current
    if let Ok(Some(lane)) = ctx.store.get_lane(run_id, idx).await {
        if let Some(cur) = LaneState::parse(&lane.state) {
            if !cur.can_transition_to(next) && cur != next {
                let _ = ctx
                    .store
                    .append_lane_event(
                        run_id,
                        idx,
                        now_ms(),
                        "log",
                        &format!(
                            "warn: transition {}->{} not in table",
                            cur.as_str(),
                            next.as_str()
                        ),
                    )
                    .await;
            }
        }
    }
    ctx.store
        .update_lane_state(run_id, idx, next.as_str())
        .await
        .map_err(|e| e.to_string())?;
    let _ = ctx
        .store
        .append_lane_event(run_id, idx, now_ms(), "state", next.as_str())
        .await;
    publish(ctx, run_id).await;
    Ok(())
}

pub async fn publish(ctx: &PipelineCtx, run_id: &str) {
    if let (Ok(Some(run)), Ok(lanes)) = (
        ctx.store.get_run(run_id).await,
        ctx.store.list_lanes(run_id).await,
    ) {
        let snap = snapshot::snapshot_for(&run, &lanes, now_ms());
        ctx.sse.publish(snap);
    }
}

fn short_run_id(run_id: &str) -> String {
    run_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect()
}

fn lane_workdir(ctx: &PipelineCtx, run_id: &str, idx: i64) -> PathBuf {
    ctx.config
        .data_dir
        .join("work")
        .join(run_id)
        .join(format!("lane-{idx}"))
}

/// Sprite-side (relative) workdir for real harness runs; the command cd's into
/// it via the SWARM_WORKDIR env so mock and real execs share one convention.
fn sprite_workdir(run_id: &str, idx: i64) -> String {
    format!("swarm-work/{}/lane-{idx}", short_run_id(run_id))
}

/// Tail of at most 4096 chars, cut on a char boundary (untrusted output must
/// never panic).
fn tail_4k(s: &str) -> String {
    if s.len() <= 4096 {
        return s.to_owned();
    }
    let mut start = s.len() - 4096;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

/// Gate the LAST balanced JSON object in the output (the agent's final answer),
/// trying candidates back-to-front. Never exposes raw output in errors.
fn gate_last_json<T: serde::de::DeserializeOwned>(
    role: Role,
    text: &str,
    opts: &GateOptions,
) -> Result<T, String> {
    let mut last_err: Option<String> = None;
    for cand in harness::json_candidates(text).into_iter().rev() {
        match gate_with_options(role, &cand, opts)
            .map_err(|e| e.to_string())
            .and_then(|_| parse::<T>(role, &cand).map_err(|e| e.to_string()))
        {
            Ok(v) => return Ok(v),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| "no JSON object found".to_owned()))
}

/// #21: injection markers must actually act — flag them as a lane event and
/// downgrade the lane to a failure. Brief/agent content is DATA, never
/// instructions.
async fn gate_injection_free(
    role: Role,
    run_id: &str,
    idx: i64,
    text: &str,
    ctx: &PipelineCtx,
) -> Result<(), String> {
    let markers = match serde_json::from_str::<Value>(text) {
        Ok(v) => prompts::injection_markers(&v),
        Err(_) => Vec::new(),
    };
    if markers.is_empty() {
        return Ok(());
    }
    let joined = markers.join(",");
    let _ = ctx
        .store
        .append_lane_event(
            run_id,
            idx,
            now_ms(),
            "injection",
            &format!("role={} paths={joined}", role.as_str()),
        )
        .await;
    Err(format!(
        "injection: marker detected in {} output (paths: {joined})",
        role.as_str()
    ))
}

/// Parse a cost figure out of the harness output when the gateway reported
/// usage.opencode.allowance inline; 0.0 when absent.
fn cost_from_output(text: &str) -> Option<f64> {
    harness::json_candidates(text)
        .iter()
        .rev()
        .find_map(|cand| {
            let v: Value = serde_json::from_str(cand).ok()?;
            let usage = v.get("usage")?;
            let oc = usage.get("opencode")?;
            match oc {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.parse().ok(),
                Value::Object(o) => ["allowance", "cost", "cost_usd", "total"]
                    .iter()
                    .find_map(|k| o.get(*k).and_then(|x| x.as_f64())),
                _ => None,
            }
        })
}

/// #20: lanes.error is a fixed class plus a short, ASCII-safe excerpt — raw
/// agent output never reaches the dashboard.
fn lane_error_message(e: &str) -> String {
    let (class, rest) = match e.find(": ") {
        Some(pos) => (&e[..pos], &e[pos + 2..]),
        None => (e, ""),
    };
    let class: String = class.chars().take(48).collect();
    if rest.is_empty() {
        return class;
    }
    let excerpt: String = rest
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                ' '
            }
        })
        .take(200)
        .collect();
    format!("{class}: {excerpt}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_4k_cuts_on_char_boundary() {
        // Multi-byte characters must never be split mid-sequence (#9).
        let s: String = std::iter::repeat("héllo\u{1F600}").take(2000).collect();
        let tail = tail_4k(&s);
        assert!(tail.len() <= 4096);
        assert!(std::str::from_utf8(tail.as_bytes()).is_ok());
        assert_eq!(tail, s[s.len() - tail.len()..]);
    }

    #[test]
    fn tail_4k_short_strings_unchanged() {
        assert_eq!(tail_4k("short"), "short");
    }

    #[test]
    fn lane_error_message_hides_raw_agent_output() {
        let raw = "implementer: schema gate failed (missing field `summary`); out={\"summary\":\"<1000 chars of agent output>\"}";
        let msg = lane_error_message(raw);
        assert!(msg.starts_with("implementer: "));
        assert!(msg.len() <= 260, "message must be short: {msg}");
        // non-ASCII agent output is dropped
        let msg2 = lane_error_message("validator: deadline exceeded — é");
        assert!(msg2.chars().all(|c| c.is_ascii_graphic() || c == ' '));
        assert_eq!(lane_error_message("plain"), "plain");
    }

    #[test]
    fn gate_last_json_prefers_the_final_object() {
        let text = "chatter {\"wrong\":true} final {\"summary\":\"ok\",\"files\":2,\"run_command\":\"x\",\"done\":true,\"blockers\":[]}";
        let out: ImplementerOutput =
            gate_last_json(Role::Implementer, text, &GateOptions::default()).unwrap();
        assert_eq!(out.summary, "ok");
        // garbage input errors without leaking the raw text
        let err = gate_last_json::<ImplementerOutput>(
            Role::Implementer,
            "no json at all",
            &GateOptions::default(),
        )
        .unwrap_err();
        assert_eq!(err, "no JSON object found");
    }

    #[test]
    fn cost_from_output_parses_usage_allowance() {
        let text = r#"noise {"summary":"s","usage":{"opencode":{"allowance":0.25}}}"#;
        assert_eq!(cost_from_output(text), Some(0.25));
        assert_eq!(cost_from_output("nothing"), None);
    }
}
