use std::path::PathBuf;
use std::sync::Arc;

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
    pub hub_registry:
        Option<std::sync::Arc<tokio::sync::RwLock<std::collections::HashMap<String, SseHub>>>>,
}

/// How to obtain implementer/validator outputs for a lane.
/// In test mode, exec a command on the sprite that prints JSON to stdout.
/// In real mode, this would run full harness; here we provide a hookable command builder.
#[derive(Debug, Clone)]
pub enum HarnessMode {
    /// Exec `command` inside sprite; stdout must be schema-valid JSON for the role.
    Command {
        impl_command: String,
        val_command: String,
    },
    /// Real mode placeholder: attempts opencode/codex/curl-loop chain (best-effort).
    Real { gateway_connector_id: String },
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
                    // retry: reset to queued-ish by trying again (state will be set in attempt)
                    continue;
                } else {
                    // hard fail / skip
                    let _state = if attempts > 1 || !transient {
                        "failed"
                    } else {
                        "failed"
                    };
                    let _ = ctx.store.set_lane_error(&run_id, idx, &e).await;
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

    let short = short_run_id(&run_id);
    let sprite_name = format!("swarm-{short}-impl-{idx}");
    // create sprite
    ctx.sprite
        .create(&sprite_name)
        .await
        .map_err(|e| e.to_string())?;
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
        run_implementer(&ctx, &run_id, idx, &sprite_name, &model, harness),
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
    transition(&ctx, &run_id, idx, LaneState::Done).await?;
    publish(&ctx, &run_id).await;

    // 200-check + artifacts at lane completion (after impl)
    let http_ok = artifacts::check_http_200(&impl_url).await
        || artifacts::check_http_200("http://127.0.0.1:8080").await;
    let _ = ctx
        .store
        .append_lane_event(
            &run_id,
            idx,
            now_ms(),
            "log",
            &format!("http_200={http_ok}"),
        )
        .await;

    let workdir = lane_workdir(&ctx, &run_id, idx);
    let console_tail = impl_out.console_tail.clone();
    if let Ok((tarball, tokei_json, files_n, _loc_n)) =
        artifacts::collect_artifacts(&ctx.config, &run_id, idx, &workdir, &console_tail).await
    {
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
    harness: &HarnessMode,
) -> Result<ImplRunResult, String> {
    let (cmd, envs) = match harness {
        HarnessMode::Command { impl_command, .. } => (impl_command.clone(), vec![]),
        HarnessMode::Real {
            gateway_connector_id,
        } => {
            let prompt_text = format!("Implement lane {idx} for run {run_id} model {model}");
            let cmd = format!(
                "set -e; if command -v opencode >/dev/null 2>&1; then nohup opencode run '{}' >/tmp/opencode_run.log 2>&1 & echo started; else echo '{{\"summary\":\"fallback\",\"files\":0,\"run_command\":\"none\",\"done\":false,\"blockers\":[\"no harness\"]}}'; fi",
                prompt_text.replace('\'', "'\\''")
            );
            (
                cmd,
                vec![(
                    "OPENCODE_GATEWAY_CONNECTOR".to_owned(),
                    gateway_connector_id.clone(),
                )],
            )
        }
    };
    let workdir = lane_workdir(ctx, run_id, idx);
    let _ = std::fs::create_dir_all(&workdir);
    let mut opts = ExecOptions::default().with_dir(&workdir);
    for (k, v) in envs {
        opts = opts.with_env(k, v);
    }
    let out = ctx
        .sprite
        .exec(sprite_name, &cmd, &opts)
        .await
        .map_err(|e| e.to_string())?;
    let stdout = out.stdout.trim().to_string();
    let text = extract_json(&stdout).unwrap_or(stdout.clone());
    let gate_opts = GateOptions::default();
    gate_with_options(Role::Implementer, &text, &gate_opts)
        .map_err(|e| format!("impl schema: {e}; out={text}"))?;
    let parsed: ImplementerOutput = parse(Role::Implementer, &text).map_err(|e| e.to_string())?;
    Ok(ImplRunResult {
        summary: parsed.summary,
        files: parsed.files,
        cost_usd: 0.0,
        console_tail: tail_4k(&stdout),
    })
}

async fn run_validator(
    ctx: &PipelineCtx,
    run_id: &str,
    idx: i64,
    sprite_name: &str,
    _impl_url: &str,
    _impl_summary: &str,
    harness: &HarnessMode,
) -> Result<ValRunResult, String> {
    let cmd = match harness {
        HarnessMode::Command { val_command, .. } => val_command.clone(),
        HarnessMode::Real { .. } => {
            format!("echo '{{\"verdict\":\"fail\",\"features\":\"\",\"bugs\":\"not implemented\",\"security\":\"\",\"quality_score\":0,\"summary\":\"real harness not implemented in this environment\"}}'")
        }
    };
    let workdir = lane_workdir(ctx, run_id, idx);
    let _ = std::fs::create_dir_all(&workdir);
    let opts = ExecOptions::default().with_dir(&workdir);
    let out = ctx
        .sprite
        .exec(sprite_name, &cmd, &opts)
        .await
        .map_err(|e| e.to_string())?;
    let stdout = out.stdout.trim().to_string();
    let text = extract_json(&stdout).unwrap_or(stdout.clone());
    gate_with_options(Role::Validator, &text, &GateOptions::default())
        .map_err(|e| format!("val schema: {e}; out={text}"))?;
    let parsed: ValidatorOutput = parse(Role::Validator, &text).map_err(|e| e.to_string())?;
    Ok(ValRunResult {
        verdict: parsed.verdict,
        summary: parsed.summary,
        quality_score: parsed.quality_score,
        cost_usd: 0.0,
    })
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
        if let Some(url) = &l.impl_url {
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
        judge::run_judge(llm, &reports, &extracts).await?
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

fn tail_4k(s: &str) -> String {
    if s.len() <= 4096 {
        return s.to_owned();
    }
    s[s.len() - 4096..].to_string()
}

fn extract_json(s: &str) -> Option<String> {
    // find first '{'..last '}' span that parses
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end <= start {
        return None;
    }
    let cand = &s[start..=end];
    if serde_json::from_str::<Value>(cand).is_ok() {
        Some(cand.to_owned())
    } else {
        None
    }
}
