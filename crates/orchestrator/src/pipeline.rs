use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use llm_gateway::{MachineLlmClient, IMPLEMENTER_MODEL, VALIDATOR_MODEL};
use prompts::{
    gate_and_scan_injections, parse, BriefFulfillment, BriefFulfillmentState, GateOptions,
    ImplementerOutput, JudgeOutput, Role, ValidatorOutput, Verdict,
};
use serde_json::Value;
use sprites_client::{ExecOptions, SpriteClient};
use store::{now_ms, Run, Store};

use crate::artifacts;
use crate::config::{ms_after, PipelineConfig};
use crate::decompose::{self, MAX_DECOMPOSE_CASES};
use crate::evidence;
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
            gateway_connector_id: crate::config::DEFAULT_GATEWAY_CONNECTOR_ID.to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub brief: String,
    /// DEPRECATED (Wave B): the brief-decomposition output decides the lane
    /// count; this field is only kept for the API validation contract and
    /// drives the deterministic no-LLM template fallback. Decomposition output
    /// always wins when a machine LLM is available.
    pub n_lanes: usize,
    pub harness: HarnessMode,
}

pub async fn start_run(ctx: Arc<PipelineCtx>, req: RunRequest) -> Result<Run, String> {
    PipelineConfig::validate_n(req.n_lanes)?;
    let created = now_ms();
    let run = ctx
        .store
        .create_run(&req.brief, req.n_lanes as i64, created)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(reg) = &ctx.hub_registry {
        let mut hubs = reg.write().await;
        hubs.insert(run.id.clone(), ctx.sse.clone());
    }

    // Wave B: brief decomposition decides the cases (and so the lane count).
    let (cases, decompose_cost, decompose_log, truncated_from) =
        generate_cases(&ctx, &run.id, &req.brief, req.n_lanes).await;
    ctx.store
        .set_run_n_lanes(&run.id, cases.len() as i64)
        .await
        .map_err(|e| e.to_string())?;
    for (idx, case) in cases.iter().enumerate() {
        let vjson = serde_json::to_string(case).unwrap_or_else(|_| "{}".to_owned());
        ctx.store
            .insert_lane(&run.id, idx as i64, &vjson)
            .await
            .map_err(|e| e.to_string())?;
    }
    // Decomposition diagnostics land on lane 0's event log: no lanes existed
    // while the attempts ran, so retry/fallback notes are appended here.
    for line in &decompose_log {
        let _ = ctx
            .store
            .append_lane_event(&run.id, 0, now_ms(), "log", line)
            .await;
    }
    if let Some(original) = truncated_from {
        let _ = ctx
            .store
            .append_lane_event(
                &run.id,
                0,
                now_ms(),
                "log",
                &format!(
                    "decomposition truncated from {original} to {MAX_DECOMPOSE_CASES} cases (hard cap)"
                ),
            )
            .await;
    }
    ctx.store
        .update_run_status(&run.id, "running")
        .await
        .map_err(|e| e.to_string())?;
    publish(&ctx, &run.id).await;

    let run_id = run.id.clone();
    let ctx_bg = ctx.clone();
    let lane_count = cases.len();
    tokio::spawn(async move {
        if let Err(e) =
            execute_run(ctx_bg.clone(), run_id.clone(), req.harness, decompose_cost).await
        {
            let _ = ctx_bg.store.update_run_status(&run_id, "failed").await;
            let ts = now_ms();
            for i in 0..lane_count as i64 {
                let _ = ctx_bg
                    .store
                    .append_lane_event(&run_id, i, ts, "log", &format!("run_failed: {e}"))
                    .await;
            }
            publish(&ctx_bg, &run_id).await;
            drop_hub(&ctx_bg, &run_id).await;
        }
    });

    ctx.store
        .get_run(&run.id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "run missing".to_owned())
}

/// Wave B: produce the per-lane case values. With a machine LLM the brief
/// decomposition decides the cases; without one the deterministic template
/// fallback keeps the pre-decomposition behavior (requested lane count).
async fn generate_cases(
    ctx: &PipelineCtx,
    run_id: &str,
    brief: &str,
    n_requested: usize,
) -> (Vec<Value>, Option<f64>, Vec<String>, Option<usize>) {
    let Some(llm) = &ctx.machine_llm else {
        let cases = prompts::template_variants(n_requested)
            .into_iter()
            .map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
            .collect::<Vec<Value>>();
        return (cases, Some(0.0), Vec::new(), None);
    };
    let outcome = decompose::decompose_brief(llm, run_id, brief).await;
    (
        outcome.cases,
        outcome.cost,
        outcome.log,
        outcome.truncated_from,
    )
}

async fn execute_run(
    ctx: Arc<PipelineCtx>,
    run_id: String,
    harness: HarnessMode,
    decompose_cost: Option<f64>,
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

    // Judge if >=2 lanes passed validation (validator fail verdicts and
    // failed lanes are excluded from the judge set).
    let lanes = ctx
        .store
        .list_lanes(&run_id)
        .await
        .map_err(|e| e.to_string())?;
    let validated: Vec<_> = lanes
        .iter()
        .filter(|l| l.state == "validated" && l.val_verdict.as_deref() == Some("pass"))
        .cloned()
        .collect();
    if validated.len() >= 2 {
        run_judge_phase(&ctx, &run_id, &validated, decompose_cost).await?;
    } else {
        // mark run failed if not enough
        let _ = ctx.store.update_run_status(&run_id, "failed").await;
    }

    publish(&ctx, &run_id).await;
    // The run is terminal (judged or failed): drop its hub from the registry
    // so the per-run broadcast channel is not leaked (audit R3 finding 5).
    // Subscribers already holding a receiver still drain the final snapshot
    // before the channel closes.
    drop_hub(&ctx, &run_id).await;
    Ok(())
}

/// Remove the run's hub from the registry once the run cannot change anymore.
/// The terminal snapshot was already broadcast by `publish`; dropping the
/// sender closes the channel for late subscribers, and the web layer falls
/// back to the DB snapshot path for them.
async fn drop_hub(ctx: &PipelineCtx, run_id: &str) {
    if let Some(reg) = &ctx.hub_registry {
        reg.write().await.remove(run_id);
    }
}

async fn run_lane(
    ctx: Arc<PipelineCtx>,
    run_id: String,
    idx: i64,
    harness: HarnessMode,
) -> Result<(), String> {
    let mut attempts = 0;
    // R3-A #4: two independent retry budgets. Lane-level failures (transient
    // infra) consume `retries_per_lane`; harness-attributable failures (probe,
    // exec, schema gate from a broken harness) get their own budget = chain
    // length so the chain can be walked to the curl floor without ever
    // burning the lane retry.
    let max_lane_retries = ctx.config.retries_per_lane;
    let max_harness_retries = harness::HARNESS_CHAIN.len();
    let mut lane_retries = 0;
    let mut harness_retries = 0;

    // --- Implementer phase (F9): sprite creation + implementer session +
    // 200 gate + artifacts. ONLY implementer-phase failures destroy the
    // sprite and recreate it.
    let phase = loop {
        attempts += 1;
        let res = run_implementer_phase(ctx.clone(), run_id.clone(), idx, &harness).await;
        match res {
            Ok(p) => break p,
            Err(e) => {
                let harness_failure = is_harness_error(&e);
                let transient = is_transient_error(&e);
                let retry_lane = !harness_failure && transient && lane_retries < max_lane_retries;
                let retry_harness = harness_failure && harness_retries < max_harness_retries;
                let ts = now_ms();
                let _ = ctx
                    .store
                    .append_lane_event(
                        &run_id,
                        idx,
                        ts,
                        "log",
                        &format!(
                            "lane attempt {attempts} failed{}: {e}",
                            if harness_failure {
                                " (harness escalation)"
                            } else {
                                ""
                            }
                        ),
                    )
                    .await;
                if retry_lane || retry_harness {
                    if retry_harness {
                        harness_retries += 1;
                    } else {
                        lane_retries += 1;
                    }
                    // #8: a retry cannot succeed while the previous sprite
                    // still exists (deterministic name, real `sprite create`
                    // rejects duplicates). Destroy --force it and reset the
                    // lane so the next attempt starts clean.
                    let sprite_name = ctx
                        .store
                        .get_lane(&run_id, idx)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|l| l.sprite_name);
                    if let Some(name) = &sprite_name {
                        let _ = ctx.sprite.destroy(name).await;
                    }
                    let _ = ctx.store.set_lane_error(&run_id, idx, "").await;
                    // The harness choice may have been an install failure;
                    // invalidate so the next attempt re-probes the chain.
                    if let Some(name) = &sprite_name {
                        if retry_harness {
                            // R3-A #4: blacklist the choice this attempt used
                            // (Fatal harness errors never blacklisted it
                            // inside run_prompt), so the retry walks the
                            // chain instead of re-picking the same member.
                            if let Some(used) = ctx.harness_cache.last_used().await {
                                ctx.harness_cache.invalidate_choice(name, used).await;
                            } else {
                                ctx.harness_cache.invalidate(name).await;
                            }
                        } else {
                            ctx.harness_cache.invalidate(name).await;
                        }
                    }
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
    };
    let ImplPhaseOutcome {
        sprite_name,
        impl_url,
        impl_out,
        tarball_bytes,
    } = phase;

    // --- Validator phase (F9): failures retry IN PLACE. A validator-phase
    // failure must NOT recreate the implementer's sprite or rerun the
    // implementer (run 7: one schema-gate hiccup escalated the harness and
    // recreated the sprite, burning a good implementer build). New validator
    // session on the same sprite; the site and workdir stay intact.
    let max_val_retries = ctx.config.retries_per_lane;
    let mut val_retries = 0;
    loop {
        let res = run_validator_phase(
            &ctx,
            &run_id,
            idx,
            &sprite_name,
            &impl_url,
            &impl_out,
            tarball_bytes.as_deref(),
            &harness,
        )
        .await;
        match res {
            Ok(()) => return Ok(()),
            Err(e) => {
                let ts = now_ms();
                if val_retries < max_val_retries {
                    val_retries += 1;
                    let _ = ctx
                        .store
                        .append_lane_event(
                            &run_id,
                            idx,
                            ts,
                            "log",
                            &format!(
                                "validator attempt {} failed; retrying in place (sprite and site kept): {e}",
                                val_retries
                            ),
                        )
                        .await;
                    continue;
                }
                // retries exhausted: hard fail / skip
                let _ = ctx
                    .store
                    .set_lane_error(&run_id, idx, &lane_error_message(&e))
                    .await;
                let _ = transition(&ctx, &run_id, idx, LaneState::Failed).await;
                let _ = ctx.sprite.destroy(&sprite_name).await;
                publish(&ctx, &run_id).await;
                return Err(e);
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

/// R2-2: errors attributable to the coding-agent harness chain (agent exit,
/// unusable schema output, curl-floor exhaustion). They escalate on retry:
/// the failed choice is blacklisted and the next chain member is probed,
/// within the existing per-lane retry budget.
fn is_harness_error(e: &str) -> bool {
    let l = e.to_lowercase();
    l.contains("harness[") || l.contains("schema gate failed") || l.contains("curl floor")
}

/// F6: the real public 200-check, with one auth-wall recovery round. Three
/// real runs died because the lane sprite's public routing answered 302/401/403
/// even though the implementer had done everything right — a transient routing
/// propagation gap must not kill an hour of agent work. When the failed probe
/// carries an auth-wall signature (30x/401/403), re-issue
/// `sprite config update -s <name> --url-auth public` (asserted + logged, see
/// `assert_public_url_auth`), verify `sprite info` reports auth=public, let
/// routing settle briefly, and re-probe exactly once. Any other failure (or a
/// still-private URL after the retry) fails the lane as before.
async fn ensure_public_200(
    ctx: &Arc<PipelineCtx>,
    run_id: &str,
    idx: i64,
    impl_url: &str,
    sprite_name: &str,
) -> Result<(), String> {
    // Single probe before deciding: a double probe would burn the wall's
    // one-shot response and misread a still-private URL as healthy.
    match artifacts::fetch_public_status(impl_url, ctx.config.probe_allow_loopback).await {
        Ok(status) if (200..300).contains(&status) => return Ok(()),
        Ok(status) if artifacts::is_auth_wall_status(status) => {
            let _ = ctx
                .store
                .append_lane_event(
                    run_id,
                    idx,
                    now_ms(),
                    "log",
                    &format!(
                        "200-check hit an auth wall (HTTP {status}); re-issuing url-auth public and retrying once",
                    ),
                )
                .await;
        }
        Ok(status) => {
            return Err(format!(
                "implementer: live URL did not answer 200 (HTTP {status}): {impl_url}"
            ));
        }
        Err(_) => {
            return Err(format!(
                "implementer: live URL did not answer 200: {impl_url}"
            ));
        }
    }
    // F7: the re-issue is asserted, not best-effort: the config update output
    // is captured and logged, and `sprite info` must report auth=public
    // before the re-probe. A config failure here fails the lane with the
    // config error instead of a misleading 200 failure.
    assert_public_url_auth(ctx, run_id, idx, sprite_name).await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    if !artifacts::check_http_200_policy(impl_url, ctx.config.probe_allow_loopback).await {
        return Err(format!(
            "implementer: live URL did not answer 200 after url-auth retry: {impl_url}"
        ));
    }
    Ok(())
}

/// F7: the url-auth public step is asserted, not assumed (run 5, lane 0 died
/// silently here). Issue `sprite config update -s <name> --url-auth public`,
/// capturing and logging stdout+stderr to lane_events (kind="log",
/// sanitized); on non-zero exit or empty output, retry up to
/// `URL_AUTH_ATTEMPTS` times with backoff (1s/3s), each attempt logged.
/// Authoritative verification then parses the `Auth:` field from
/// `sprite info`: the lane only proceeds when it reads `public` (the parsed
/// value is logged too). If it is still not public after the retries, the
/// lane fails EARLY with a clear error naming the config failure — before
/// any implementer session is launched, so an hour of agent work is never
/// spent on a sprite that can never pass the 200-check.
const URL_AUTH_ATTEMPTS: usize = 3;

async fn assert_public_url_auth(
    ctx: &PipelineCtx,
    run_id: &str,
    idx: i64,
    sprite_name: &str,
) -> Result<(), String> {
    let backoffs = [Duration::from_secs(1), Duration::from_secs(3)];
    let mut last_detail = "no attempt completed".to_owned();
    for attempt in 1..=URL_AUTH_ATTEMPTS {
        last_detail = update_and_verify_public_auth(ctx, run_id, idx, sprite_name, attempt).await;
        if last_detail.is_empty() {
            return Ok(());
        }
        if attempt < URL_AUTH_ATTEMPTS {
            tokio::time::sleep(backoffs[attempt - 1]).await;
        }
    }
    Err(format!(
        "url-auth: sprite {sprite_name} still not public after {URL_AUTH_ATTEMPTS} attempts ({last_detail})"
    ))
}

/// One round of `config update` + `sprite info` verification. Returns an
/// empty string on verified success, otherwise a short failure detail that
/// names what went wrong (config update exit/output or the parsed Auth
/// value). Every step is logged to lane_events, sanitized: CLI output is
/// untrusted text.
async fn update_and_verify_public_auth(
    ctx: &PipelineCtx,
    run_id: &str,
    idx: i64,
    sprite_name: &str,
    attempt: usize,
) -> String {
    let config = match ctx.sprite.set_url_auth_public(sprite_name).await {
        Ok(out) => {
            let mut combined = format!("stdout={}", out.stdout.trim());
            if !out.stderr.trim().is_empty() {
                combined.push_str(&format!(" stderr={}", out.stderr.trim()));
            }
            let _ = ctx
                .store
                .append_lane_event(
                    run_id,
                    idx,
                    now_ms(),
                    "log",
                    &format!(
                        "url-auth config update attempt {attempt} exit={}: {}",
                        out.exit_code,
                        harness::sanitize_excerpt(&combined, 300)
                    ),
                )
                .await;
            if out.stdout.trim().is_empty() && out.stderr.trim().is_empty() {
                // Empty output: success is unprovable. The `sprite info`
                // verification below still runs, but the attempt is treated
                // as failed for the retry decision.
                return format!("config update attempt {attempt} produced no output");
            }
            out
        }
        Err(e) => {
            let _ = ctx
                .store
                .append_lane_event(
                    run_id,
                    idx,
                    now_ms(),
                    "log",
                    &format!(
                        "url-auth config update attempt {attempt} failed: {}",
                        harness::sanitize_excerpt(&e.to_string(), 300)
                    ),
                )
                .await;
            return format!("config update attempt {attempt} failed: {e}");
        }
    };
    // Authoritative verification: the parsed `Auth:` value decides, not the
    // config update's own output.
    match ctx.sprite.auth(sprite_name).await {
        Ok(auth) if auth.eq_ignore_ascii_case("public") => {
            let _ = ctx
                .store
                .append_lane_event(
                    run_id,
                    idx,
                    now_ms(),
                    "log",
                    &format!(
                        "url-auth public verified via sprite info (auth={auth}, config exit={})",
                        config.exit_code
                    ),
                )
                .await;
            String::new()
        }
        Ok(auth) => {
            let _ = ctx
                .store
                .append_lane_event(
                    run_id,
                    idx,
                    now_ms(),
                    "log",
                    &format!("sprite info auth={auth} after config update attempt {attempt}"),
                )
                .await;
            format!("sprite info auth={auth} after config update attempt {attempt}")
        }
        Err(e) => {
            let _ = ctx
                .store
                .append_lane_event(
                    run_id,
                    idx,
                    now_ms(),
                    "log",
                    &format!(
                        "sprite info verification failed after config update attempt {attempt}: {}",
                        harness::sanitize_excerpt(&e.to_string(), 300)
                    ),
                )
                .await;
            format!("sprite info verification failed: {e}")
        }
    }
}

/// Outcome of the implementer phase: everything the validator phase needs
/// (F9 phase split — validator failures must not touch any of it).
struct ImplPhaseOutcome {
    sprite_name: String,
    impl_url: String,
    impl_out: ImplRunResult,
    /// F10: the bytes of the SAME source.tar.gz the orchestrator pulled at
    /// lane completion (user download artifact) — the validator's evidence
    /// is derived from it, never from a second file-collection pass. None =
    /// the artifact pull failed (evidence degrades explicitly).
    tarball_bytes: Option<Vec<u8>>,
}

async fn run_implementer_phase(
    ctx: Arc<PipelineCtx>,
    run_id: String,
    idx: i64,
    harness: &HarnessMode,
) -> Result<ImplPhaseOutcome, String> {
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

    let sprite_name = lane_sprite_name(&run_id, idx);
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
    // R3-A #2 / F7: url-auth public is scoped to the sprite (-s <name>) and
    // is ASSERTED here, before the implementer session is launched: the
    // config update is retried with captured output, and `sprite info` must
    // report auth=public. A private URL can never pass the 200-check, serve
    // the validator, or be iframed, so failing early (with a clear error
    // naming the config failure) beats silently spending the implementer
    // hour on a dead lane. The sprite name is only persisted by start_impl
    // (which we never reach on this path), so destroy it here — a leaked
    // sprite is a billable resource.
    if let Err(e) = assert_public_url_auth(&ctx, &run_id, idx, &sprite_name).await {
        let _ = ctx.sprite.destroy(&sprite_name).await;
        return Err(e);
    }
    // R3-A #1: the real host carries an opaque platform suffix; parse it from
    // `sprite info` instead of fabricating `https://{name}.sprites.app`.
    let impl_url = ctx
        .sprite
        .url(&sprite_name)
        .await
        .map_err(|e| format!("sprite url: {e}"))?;

    let started = now_ms();
    let deadline = ms_after(started, ctx.config.impl_deadline);
    let model = IMPLEMENTER_MODEL.to_owned(); // pinned: every impl lane glm-5.3-flash
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
    // R3-A #5: unknown cost stays NULL in the store (rendered as "—"); only
    // known costs are persisted and sparklined.
    if let Some(cost) = impl_out.cost_usd {
        ctx.store.add_lane_cost(&run_id, idx, cost).await.ok();
    }
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
    // a passing lane). F2: the probe is SSRF-safe (shared net.rs machinery);
    // the loopback escape is config-driven and test-only.
    if !ctx.config.test_mode {
        ensure_public_200(&ctx, &run_id, idx, &impl_url, &sprite_name).await?;
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
    // F10: the pulled tarball's bytes feed the validator's evidence — read
    // them back from the one artifact so every consumer shares it.
    let tarball_bytes = match &collected {
        Ok((tarball_path, ..)) => tokio::fs::read(tarball_path).await.ok(),
        Err(_) => None,
    };
    if let Ok((tarball, tokei_json, files_n, _loc_n)) = collected {
        let files_store = files_n.or(Some(impl_out.files as i64));
        let _ = ctx
            .store
            // F1: the screenshot columns are owned by the queue; a pre-render
            // made during the long impl session must survive completion.
            .set_artifacts(&run_id, idx, &tarball, &tokei_json, &console_tail)
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
            .set_artifacts(&run_id, idx, "", &tokei_json, &console_tail)
            .await;
    }

    // #28: checkpoint the sprite after the implementer is done, before
    // validation, so validation/retries can restart from a known state.
    let _ = ctx.sprite.checkpoint(&sprite_name).await;

    Ok(ImplPhaseOutcome {
        sprite_name,
        impl_url,
        impl_out,
        tarball_bytes,
    })
}

#[allow(clippy::too_many_arguments)] // phase fn mirrors the pipeline's call shape
async fn run_validator_phase(
    ctx: &PipelineCtx,
    run_id: &str,
    idx: i64,
    sprite_name: &str,
    impl_url: &str,
    impl_out: &ImplRunResult,
    tarball_bytes: Option<&[u8]>,
    harness: &HarnessMode,
) -> Result<(), String> {
    // --- Validator phase: acquires its own slot ---
    let _val_permit = ctx.slots.acquire().await;
    let val_started = now_ms();
    let val_deadline = ms_after(val_started, ctx.config.val_deadline);
    // Wave A pinning: the reviewer is deepseek-v4.1-flash — a different model
    // than the implementer (glm-5.3-flash), never same-model self-review.
    let val_model = VALIDATOR_MODEL.to_owned();
    ctx.store
        .start_validation(run_id, idx, &val_model, val_started, val_deadline)
        .await
        .map_err(|e| e.to_string())?;
    let _ = ctx
        .store
        .append_lane_event(run_id, idx, val_started, "state", "validating")
        .await;
    publish(ctx, run_id).await;

    let val_result = tokio::time::timeout(
        ctx.config.val_deadline,
        run_validator(
            ctx,
            run_id,
            idx,
            sprite_name,
            impl_url,
            &impl_out.summary,
            tarball_bytes,
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
            run_id,
            idx,
            verdict_str,
            &val_out.summary,
            val_out.quality_score,
        )
        .await
        .map_err(|e| e.to_string())?;
    if let Some(cost) = val_out.cost_usd {
        ctx.store.add_lane_cost(run_id, idx, cost).await.ok();
    }
    // R2 finding 5: a validator `verdict: fail` must demote the lane out of
    // the judge set. The report stays persisted (val_verdict, val_report_json)
    // so the detail view keeps showing it; the lane is failed-with-report.
    if val_out.verdict == Verdict::Fail {
        let _ = ctx
            .store
            .append_lane_event(
                run_id,
                idx,
                now_ms(),
                "log",
                "validator verdict: fail; lane excluded from judge set",
            )
            .await;
        let _ = ctx
            .store
            .set_lane_error(run_id, idx, "validator: verdict fail (report retained)")
            .await;
        transition(ctx, run_id, idx, LaneState::Failed).await?;
        publish(ctx, run_id).await;
        return Ok(());
    }
    transition(ctx, run_id, idx, LaneState::Validated).await?;
    publish(ctx, run_id).await;

    Ok(())
}

#[derive(Debug, Clone)]
struct ImplRunResult {
    summary: String,
    files: u32,
    run_command: String,
    /// Gateway-reported cost; None = unknown (R3-A #5: persist NULL, never
    /// fabricate 0.00).
    cost_usd: Option<f64>,
    console_tail: String,
}

#[derive(Debug, Clone)]
struct ValRunResult {
    verdict: Verdict,
    summary: String,
    quality_score: f64,
    cost_usd: Option<f64>,
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
    let mut used_choice: Option<harness::HarnessChoice> = None;
    let (stdout, harness_cost) = match harness {
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
            // Test-only canned command: it provably makes no model calls, so
            // zero is the true cost, not a fabrication.
            (out.stdout, Some(0.0))
        }
        HarnessMode::Real { .. } => {
            let variant: Value = serde_json::from_str(variant_json)
                .map_err(|e| format!("implementer variant: {e}"))?;
            // Wave B: decomposition lanes carry a full self-contained
            // `impl_prompt`; template-fallback lanes keep the classic variant
            // shape. Both paths get the standard site constraints.
            let prompt = match variant.get("impl_prompt").and_then(Value::as_str) {
                Some(case_impl_prompt) => prompts::implementer_case_prompt(case_impl_prompt),
                None => prompts::implementer_prompt(&variant),
            };
            let recipe =
                llm_gateway::SpriteRecipe::for_model(format!("{run_id}-{idx}"), model.to_owned());
            let connector_env = (
                "OPENCODE_GATEWAY_CONNECTOR".to_owned(),
                ctx.config.gateway_connector_id.clone(),
            );
            let run = match harness::run_prompt(
                &ctx.harness_cache,
                &ctx.sprite,
                sprite_name,
                harness::HarnessPrompt {
                    system: &prompt.system,
                    user: &prompt.user,
                    recipe: &recipe,
                    timeout: ctx.config.impl_deadline,
                    connector_env: Some(connector_env),
                    workdir: Some(&sprite_workdir(run_id, idx)),
                    role: Role::Implementer,
                    retry_backoff: harness::GATEWAY_BACKOFF_BASE,
                },
            )
            .await
            {
                Ok(run) => run,
                // Failure round: the failed attempt's captured stdout tail is
                // logged too, so a 0-byte capture is visible in the evidence.
                // F17: the transient-gateway retry lines are logged first so
                // the evidence shows each backoff attempt with its number.
                Err(f) => {
                    if let Some(attempt) = &f.last {
                        for line in &attempt.retry_log {
                            let _ = ctx
                                .store
                                .append_lane_event(run_id, idx, now_ms(), "log", line)
                                .await;
                        }
                        let _ = ctx
                            .store
                            .append_lane_event(
                                run_id,
                                idx,
                                now_ms(),
                                "log",
                                &harness_tail_log(attempt),
                            )
                            .await;
                    }
                    return Err(format!("implementer: {}", f.msg));
                }
            };
            // F17: transient-gateway retries that preceded the success are
            // part of the run's evidence.
            for line in &run.retry_log {
                let _ = ctx
                    .store
                    .append_lane_event(run_id, idx, now_ms(), "log", line)
                    .await;
            }
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
            // Failure round: the captured stdout tail is logged even on
            // success so a 0-byte capture shows up in the tail evidence.
            let _ = ctx
                .store
                .append_lane_event(run_id, idx, now_ms(), "log", &harness_tail_log(&run))
                .await;
            used_choice = Some(run.choice);
            // R2 finding 3 / R3-A #5: the gateway-reported cost travels with
            // the run; an unknown cost stays None (never fabricated to 0.00).
            let cost = harness_cost_of(&run);
            (run.output, cost)
        }
    };
    let stdout = stdout.trim().to_string();
    let (parsed, markers) = match gate_last_json::<ImplementerOutput>(
        Role::Implementer,
        &stdout,
        &GateOptions::default(),
    ) {
        Ok(v) => v,
        Err(e) => {
            // R2-2: a schema-gate failure is harness-attributable (the agent
            // produced unusable output); blacklist the choice so the retry
            // probes the next chain member within the harness budget.
            if let Some(c) = used_choice {
                ctx.harness_cache.invalidate_choice(sprite_name, c).await;
            }
            return Err(format!(
                // the raw agent output is untrusted data: name the gate failure
                // and its size, never echo the payload itself
                "implementer: schema gate failed ({e}; output {} bytes, not stored)",
                stdout.len()
            ));
        }
    };
    // R2 finding 4 (#21 regression): injection markers must actually act.
    // An injection marker fails the lane; the error propagates through
    // run_lane (transient? no) to a hard failure.
    if !markers.is_empty() {
        let joined = markers.join(",");
        let _ = ctx
            .store
            .append_lane_event(
                run_id,
                idx,
                now_ms(),
                "injection",
                &format!("role={} paths={joined}", Role::Implementer.as_str()),
            )
            .await;
        return Err(format!(
            "injection: marker detected in {} output (paths: {joined})",
            Role::Implementer.as_str()
        ));
    }
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
    Ok(ImplRunResult {
        summary: parsed.summary,
        files: parsed.files,
        run_command: parsed.run_command,
        cost_usd: harness_cost,
        console_tail: tail_4k(&stdout),
    })
}

#[allow(clippy::too_many_arguments)] // phase fn mirrors the pipeline's call shape
async fn run_validator(
    ctx: &PipelineCtx,
    run_id: &str,
    idx: i64,
    sprite_name: &str,
    impl_url: &str,
    impl_summary: &str,
    tarball_bytes: Option<&[u8]>,
    harness: &HarnessMode,
) -> Result<ValRunResult, String> {
    let mut used_choice: Option<harness::HarnessChoice> = None;
    let (stdout, harness_cost) = match harness {
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
            // Test-only canned command: no model calls, zero is the true cost.
            (out.stdout, Some(0.0))
        }
        HarnessMode::Real { .. } => {
            // Real mode: run the real validator report prompt through the
            // harness against the actual code in the lane workdir and the
            // live site. Never fabricate a verdict. F9/F10: the validator's
            // evidence is derived by the orchestrator from the SAME
            // source.tar.gz pulled at lane completion (file list, key-file
            // excerpts — entry HTML, main sources, config) plus a live HTTP
            // probe of the public URL, all embedded as DATA. The curl-floor
            // validator has no shell, and a validator without evidence
            // writes junk verdicts (run 7) or fails on unverified claims
            // (run 8 — src/main.ts missing from the evidence).
            let workdir = sprite_workdir(run_id, idx);
            let evidence = evidence::gather_validator_evidence(
                tarball_bytes,
                impl_url,
                ctx.config.probe_allow_loopback,
            )
            .await;
            let _ = ctx
                .store
                .append_lane_event(
                    run_id,
                    idx,
                    now_ms(),
                    "log",
                    &format!(
                        "validator evidence gathered ({} bytes, embedded as DATA)",
                        evidence.len()
                    ),
                )
                .await;
            let prompt = prompts::validator_prompt(impl_summary, impl_url, &workdir, &evidence);
            let recipe = llm_gateway::SpriteRecipe::for_model(
                format!("{run_id}-{idx}-val"),
                VALIDATOR_MODEL.to_owned(),
            );
            let connector_env = (
                "OPENCODE_GATEWAY_CONNECTOR".to_owned(),
                ctx.config.gateway_connector_id.clone(),
            );
            let run = match harness::run_prompt(
                &ctx.harness_cache,
                &ctx.sprite,
                sprite_name,
                harness::HarnessPrompt {
                    system: &prompt.system,
                    user: &prompt.user,
                    recipe: &recipe,
                    timeout: ctx.config.val_deadline,
                    connector_env: Some(connector_env),
                    workdir: Some(&sprite_workdir(run_id, idx)),
                    role: Role::Validator,
                    retry_backoff: harness::GATEWAY_BACKOFF_BASE,
                },
            )
            .await
            {
                Ok(run) => run,
                // Failure round: the failed attempt's captured stdout tail is
                // logged too, so a 0-byte capture is visible in the evidence.
                // F17: the transient-gateway retry lines are logged first so
                // the evidence shows each backoff attempt with its number.
                Err(f) => {
                    if let Some(attempt) = &f.last {
                        for line in &attempt.retry_log {
                            let _ = ctx
                                .store
                                .append_lane_event(run_id, idx, now_ms(), "log", line)
                                .await;
                        }
                        let _ = ctx
                            .store
                            .append_lane_event(
                                run_id,
                                idx,
                                now_ms(),
                                "log",
                                &harness_tail_log(attempt),
                            )
                            .await;
                    }
                    return Err(format!("validator: {}", f.msg));
                }
            };
            // F17: transient-gateway retries that preceded the success are
            // part of the run's evidence.
            for line in &run.retry_log {
                let _ = ctx
                    .store
                    .append_lane_event(run_id, idx, now_ms(), "log", line)
                    .await;
            }
            used_choice = Some(run.choice);
            // Failure round: the captured stdout tail is logged even on
            // success so a 0-byte capture shows up in the tail evidence.
            let _ = ctx
                .store
                .append_lane_event(run_id, idx, now_ms(), "log", &harness_tail_log(&run))
                .await;
            // R2 finding 3 / R3-A #5: gateway-reported cost; unknown stays None.
            let cost = harness_cost_of(&run);
            (run.output, cost)
        }
    };
    let stdout = stdout.trim().to_string();
    let (parsed, markers) = match gate_last_json::<ValidatorOutput>(
        Role::Validator,
        &stdout,
        &GateOptions::default(),
    ) {
        Ok(v) => v,
        Err(e) => {
            // R2-2: a schema-gate failure is harness-attributable; blacklist
            // the choice so the retry probes the next chain member.
            if let Some(c) = used_choice {
                ctx.harness_cache.invalidate_choice(sprite_name, c).await;
            }
            return Err(format!("validator: schema gate failed ({e})"));
        }
    };
    // R2 finding 4: injection markers in validator output also fail the lane.
    if !markers.is_empty() {
        let joined = markers.join(",");
        let _ = ctx
            .store
            .append_lane_event(
                run_id,
                idx,
                now_ms(),
                "injection",
                &format!("role={} paths={joined}", Role::Validator.as_str()),
            )
            .await;
        return Err(format!(
            "injection: marker detected in {} output (paths: {joined})",
            Role::Validator.as_str()
        ));
    }
    // Persist the full report (features/bugs/security) so the judge and the
    // lane detail view can be hydrated from the store.
    if let Ok(report_json) = serde_json::to_string(&parsed) {
        let _ = ctx
            .store
            .set_validator_report(run_id, idx, &report_json)
            .await;
    }
    Ok(ValRunResult {
        verdict: parsed.verdict,
        summary: parsed.summary,
        quality_score: parsed.quality_score,
        cost_usd: harness_cost,
    })
}

/// #12: load each validated lane's rendered screenshot (if any) and return it
/// as a data-URL image part, aligned with the report order. Missing renders map
/// to None so the judge degrades to text-only for that lane.
async fn collect_lane_screenshot_images(
    data_dir: &std::path::Path,
    run_id: &str,
    lanes: &[store::Lane],
) -> Vec<Option<String>> {
    let mut out = Vec::with_capacity(lanes.len());
    for lane in lanes {
        let path = lane.screenshot_path.clone().unwrap_or_else(|| {
            crate::screenshot::render_dest(data_dir, run_id, lane.idx)
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

/// R2 finding 2: nothing rendered before judging meant the judge never saw any
/// screenshot. Before the judge phase, synchronously ensure a render for every
/// validated lane: a 200-check first (unreachable sites are skipped), then a
/// bounded wait on the screenshot queue. Lanes whose render fails are skipped
/// (the judge degrades to text-only for them, per #12).
const JUDGE_RENDER_WAIT: std::time::Duration = std::time::Duration::from_secs(90);

async fn ensure_judge_renders(ctx: &PipelineCtx, run_id: &str, validated: &[store::Lane]) {
    for lane in validated {
        let Some(url) = lane.impl_url.clone() else {
            continue;
        };
        if !ctx.config.test_mode
            && !artifacts::check_http_200_policy(&url, ctx.config.probe_allow_loopback).await
        {
            tracing::warn!(
                run_id,
                lane = lane.idx,
                "judge render skipped: URL did not answer 200"
            );
            continue;
        }
        let state = ctx
            .screenshots
            .ensure_rendered(run_id, lane.idx, &url, JUDGE_RENDER_WAIT)
            .await;
        if state != "done" {
            tracing::warn!(
                run_id,
                lane = lane.idx,
                "judge render not available (state {state}); lane judged without image"
            );
        }
    }
}

async fn run_judge_phase(
    ctx: &PipelineCtx,
    run_id: &str,
    validated: &[store::Lane],
    decompose_cost: Option<f64>,
) -> Result<(), String> {
    ensure_judge_renders(ctx, run_id, validated).await;
    // Wave A: the judge sees the user's ORIGINAL brief (from the run row) so
    // its verdict can carry a brief_fulfillment assessment of the winner.
    let brief = ctx
        .store
        .get_run(run_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "judge: run missing".to_owned())?
        .brief;
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
    let (verdict, judge_cost) = if let Some(llm) = &ctx.machine_llm {
        let images = collect_lane_screenshot_images(&ctx.config.data_dir, run_id, validated).await;
        judge::run_judge(
            llm,
            &llm_gateway::machine_session_id(run_id),
            &brief,
            &reports,
            &extracts,
            &images,
            ctx.config.judge_deadline,
        )
        .await?
    } else {
        // fallback deterministic judge (should not happen in proper runs; but keep safe)
        let winner = validated.first().map(|l| l.idx as usize).unwrap_or(0);
        (
            JudgeOutput {
                ranking: validated.iter().map(|l| l.idx as usize).collect(),
                winner,
                verdict_card: "Winner selected by fallback. Criteria met.".to_owned(),
                reasons: prompts::TextOrList::Text("fallback".to_owned()),
                brief_fulfillment: BriefFulfillment {
                    state: BriefFulfillmentState::Unverifiable,
                    evidence: "fallback judge (no machine LLM): brief fulfillment not assessed"
                        .to_owned(),
                },
            },
            Some(0.0),
        )
    };
    let ts = now_ms();
    // compute total cost from lanes + judge
    let lanes = ctx
        .store
        .list_lanes(run_id)
        .await
        .map_err(|e| e.to_string())?;
    // R2 finding 3: total run cost = per-lane costs + judge cost +
    // decomposition call cost (Wave B; each retry attempt's spend included).
    let mut known: Vec<f64> = lanes.iter().filter_map(|l| l.cost_usd).collect();
    known.extend(judge_cost);
    known.extend(decompose_cost);
    let total_cost = if known.is_empty() {
        None
    } else {
        Some(known.iter().sum())
    };
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
    // Wave A: persist the judge's brief-fulfillment verdict so the run page's
    // verdict card can render it (best-effort; the card text stays the
    // source of truth for the verdict itself).
    if let Ok(json) = serde_json::to_string(&verdict.brief_fulfillment) {
        let _ = ctx.store.set_judge_brief_fulfillment(run_id, &json).await;
    }
    for l in &lanes {
        let _ = ctx
            .store
            .append_lane_event(run_id, l.idx, ts, "log", "run_judged")
            .await;
    }
    // The run is terminal: nothing can change screenshot states anymore, so
    // drop cached terminal entries (R2 finding 6).
    ctx.screenshots.evict_terminal(run_id).await;
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
    // #7: the hub payload is the single source for live subscribers; the web
    // layer only rebuilds from the DB as a late-subscriber fallback.
    if let Some(snap) = snapshot::build_snapshot(&ctx.store, run_id).await {
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

/// Sprite names embed the run-id short hash, but run IDs are capability
/// tokens that keep their case while the sprite CLI only accepts
/// `^[a-z0-9][a-z0-9-]*$` ("sprite name must contain only lowercase letters,
/// numbers, and hyphens, and start/end with a letter or number"). The segment
/// is lowercased and stripped of anything outside [a-z0-9-]; the length cap of
/// `short_run_id` keeps the name short.
fn sprite_run_segment(run_id: &str) -> String {
    short_run_id(run_id)
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .collect()
}

fn lane_sprite_name(run_id: &str, idx: i64) -> String {
    format!("swarm-{}-impl-{idx}", sprite_run_segment(run_id))
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

/// Tail of at most `max` chars, cut on a char boundary (untrusted output must
/// never panic). Shared with the harness module.
fn tail_chars(s: &str, max: usize) -> String {
    harness::tail_chars(s, max)
}

/// Tail of at most 4096 chars, cut on a char boundary (untrusted output must
/// never panic).
fn tail_4k(s: &str) -> String {
    tail_chars(s, 4096)
}

/// Failure round (harness output capture): the LAST ~10KB of the captured
/// harness stdout goes into lane_events as kind="log", on success AND failure,
/// so a 0-byte capture is visible in the tail evidence instead of leaving an
/// undiagnosable "error said nothing else". F13: 10KB (up from 2KB) so a full
/// agent output that tripped the injection scan is diagnosable from the tail
/// alone. Control characters are stripped with the shared sanitizer; the
/// payload is untrusted agent output.
fn harness_tail_log(run: &harness::HarnessRun) -> String {
    let tail = harness::sanitize_excerpt(&tail_chars(&run.output, 10_240), 10_240);
    format!(
        "harness[{}] stdout ({} bytes) tail: {tail}",
        run.choice.as_str(),
        run.output.len()
    )
}

/// R2 finding 4 (#21 regression): gate the output with the SHARED
/// `prompts::gate_and_scan_injections` helper (schema gate + `[INJECTION?]`
/// scan in one step) so marker paths are surfaced with the parsed value. The
/// pipeline-local duplicate scan is gone.
fn gate_last_json<T: serde::de::DeserializeOwned>(
    role: Role,
    text: &str,
    opts: &GateOptions,
) -> Result<(T, Vec<String>), String> {
    let mut last_err: Option<String> = None;
    for cand in harness::json_candidates(text).into_iter().rev() {
        match gate_and_scan_injections(role, &cand, opts)
            .map_err(|e| e.to_string())
            .and_then(|(_value, markers)| {
                parse::<T>(role, &cand)
                    .map(|parsed| (parsed, markers))
                    .map_err(|e| e.to_string())
            }) {
            Ok(v) => return Ok(v),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| "no JSON object found".to_owned()))
}

/// R2 finding 3 / R3-A #5: the harness reports the gateway cost on the run
/// itself; None = unknown and must never be rendered as $0.00.
fn harness_cost_of(run: &harness::HarnessRun) -> Option<f64> {
    run.cost_usd
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
    let excerpt = harness::sanitize_excerpt(rest, 200);
    format!("{class}: {excerpt}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // test code: panics on failure are the assertion mechanism

    use super::*;

    #[test]
    fn tail_4k_cuts_on_char_boundary() {
        // Multi-byte characters must never be split mid-sequence (#9).
        let s: String = "héllo\u{1F600}".repeat(2000);
        let tail = tail_4k(&s);
        assert!(tail.len() <= 4096);
        assert!(std::str::from_utf8(tail.as_bytes()).is_ok());
        assert_eq!(tail, s[s.len() - tail.len()..]);
    }

    #[test]
    fn tail_4k_short_strings_unchanged() {
        assert_eq!(tail_4k("short"), "short");
    }

    /// Failure round (harness output capture): the logged stdout tail must
    /// carry the byte count (0 bytes visible in the evidence), the harness
    /// choice, and a sanitized (control-char-stripped, capped) tail.
    #[test]
    fn harness_tail_log_shows_zero_byte_and_sanitized_tails() {
        // 0-byte capture: visible, not silently empty
        let run = harness::HarnessRun {
            choice: harness::HarnessChoice::Opencode,
            output: String::new(),
            cost_usd: None,
            retry_log: Vec::new(),
        };
        let msg = harness_tail_log(&run);
        assert!(msg.contains("harness[opencode]"), "msg: {msg}");
        assert!(msg.contains("0 bytes"), "msg: {msg}");
        assert!(msg.ends_with("tail: "), "msg: {msg}");

        // control characters are stripped by the shared sanitizer
        let run = harness::HarnessRun {
            choice: harness::HarnessChoice::Curl,
            output: "ok\u{1}\u{7}done".to_owned(),
            cost_usd: Some(0.02),
            retry_log: Vec::new(),
        };
        let msg = harness_tail_log(&run);
        assert!(msg.contains("ok  done"), "msg: {msg}");
        assert!(msg.contains("8 bytes"), "msg: {msg}");

        // long outputs are capped at the last ~10KB (F13: raised from ~2KB so
        // gate failures like an injection-scan marker are fully diagnosable)
        let long = "x".repeat(20_000);
        let run = harness::HarnessRun {
            choice: harness::HarnessChoice::Claude,
            output: long,
            cost_usd: None,
            retry_log: Vec::new(),
        };
        let msg = harness_tail_log(&run);
        assert!(msg.contains("20000 bytes"), "msg: {msg}");
        assert!(
            msg.len() > 10_000 && msg.len() < 10_400,
            "tail must stay ~10KB: {}",
            msg.len()
        );
    }

    /// R2-2: harness-attributable failures escalate on retry; lane-level
    /// verdicts and infra timeouts do not.
    #[test]
    fn is_harness_error_matches_harness_failures_only() {
        assert!(is_harness_error(
            "implementer: schema gate failed (missing field `summary`)"
        ));
        assert!(is_harness_error(
            "validator: schema gate failed (invalid json)"
        ));
        assert!(is_harness_error(
            "harness[opencode]: agent exited with failure: sprite CLI `...` exited with status 1"
        ));
        assert!(is_harness_error(
            "harness[curl-loop]: no message text in response"
        ));
        assert!(!is_harness_error("implementer: deadline exceeded"));
        assert!(!is_harness_error("implementer: reported done=false"));
        assert!(!is_harness_error(
            "implementer: live URL did not answer 200"
        ));
    }

    /// The run-id short hash is a capability token that keeps its case, but
    /// the sprite CLI only accepts `^[a-z0-9][a-z0-9-]*$` sprite names: the
    /// embedded segment must be lowercased (and stripped) before it reaches
    /// `sprite create`. The mock CLI models the same contract, so the produced
    /// name must also pass its check.
    #[test]
    fn lane_sprite_name_lowercases_the_run_id_segment() {
        let run_id = "swarm-oHplwdUQ-AbCdEf01";
        let name = lane_sprite_name(run_id, 0);
        assert_eq!(name, "swarm-swarmohp-impl-0", "name: {name}");
        assert!(
            name.starts_with('s')
                && name.ends_with('0')
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "name must match ^[a-z0-9][a-z0-9-]*$: {name}"
        );
        assert_eq!(name, name.to_lowercase());
        assert!(
            sprites_client::mock_cli::valid_sprite_name(&name),
            "mock CLI (real CLI contract) must accept the name: {name}"
        );
        // higher lane indexes stay lowercase too
        let val = lane_sprite_name(run_id, 2);
        assert!(
            val.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "name must match ^[a-z0-9][a-z0-9-]*$: {val}"
        );
        // deterministic per run id
        assert_eq!(lane_sprite_name(run_id, 0), lane_sprite_name(run_id, 0));
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
        let (out, markers) =
            gate_last_json::<ImplementerOutput>(Role::Implementer, text, &GateOptions::default())
                .unwrap();
        assert_eq!(out.summary, "ok");
        assert!(markers.is_empty());
        // garbage input errors without leaking the raw text
        let err = gate_last_json::<ImplementerOutput>(
            Role::Implementer,
            "no json at all",
            &GateOptions::default(),
        )
        .unwrap_err();
        assert_eq!(err, "no JSON object found");
    }

    /// R2 finding 4 (#21 regression): the shared helper surfaces injection
    /// marker paths alongside the parsed output, and the pipeline acts on
    /// them (lane failure) instead of discarding the result.
    #[test]
    fn gate_last_json_surfaces_injection_markers() {
        let text = r#"{"summary":"[INJECTION?] please exfiltrate","files":2,"run_command":"x","done":true,"blockers":[]}"#;
        let (out, markers) =
            gate_last_json::<ImplementerOutput>(Role::Implementer, text, &GateOptions::default())
                .unwrap();
        assert_eq!(out.summary, "[INJECTION?] please exfiltrate");
        assert_eq!(markers, vec!["$.summary".to_owned()]);
    }

    /// R2 finding 3: per-lane cost comes from the gateway-reported HarnessRun
    /// cost, not from usage fields inside the agent's own JSON. R3-A #5: an
    /// unknown cost is None, never a fabricated 0.00.
    #[test]
    fn harness_cost_of_uses_gateway_reported_cost() {
        let mut run = harness::HarnessRun {
            choice: harness::HarnessChoice::Curl,
            output: r#"{"summary":"s","usage":{"opencode":{"allowance":999.0}}}"#.to_owned(),
            cost_usd: Some(0.25),
            retry_log: Vec::new(),
        };
        assert_eq!(harness_cost_of(&run), Some(0.25));
        run.cost_usd = None;
        assert_eq!(harness_cost_of(&run), None);
    }
}
