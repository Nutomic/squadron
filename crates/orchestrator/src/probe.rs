//! Wave D: worker progress probes (best-effort telemetry).
//!
//! While a lane's implementer session is running, the orchestrator probes the
//! lane sprite every `probe_interval` (~5 min) via a SEPARATE short
//! `sprite exec` — never through the lane's own harness/gateway session, so
//! the running opencode session is never disturbed. The probe asks the model
//! for a compact progress report; replies are parsed strictly and stored as
//! `progress` lane events (unparseable or empty output is silently ignored —
//! probes never fail a lane).

use std::sync::Arc;
use std::time::Duration;

use llm_gateway::{ChatRequest, SpriteRecipe, IMPLEMENTER_MODEL};
use serde_json::Value;
use sprites_client::{ExecOptions, SpriteClient};
use store::now_ms;

use crate::harness::{classify_gateway_response, json_candidates};
use crate::pipeline::publish;
use crate::pipeline::PipelineCtx;

/// Probe prompt (low effort): the model replies with compact JSON only.
pub fn probe_prompt(elapsed_min: i64, deadline_in_min: i64) -> String {
    format!(
        "Elapsed: {elapsed_min}m. Deadline in {deadline_in_min}m. Reply ONLY with compact JSON: \
         {{\"progress_pct\":<0-100>,\"stage\":\"<one short phrase>\"}}."
    )
}

/// One parsed probe report. `stage` is one short phrase (untrusted DATA, but
/// bounded and never rendered without escaping).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    pub progress_pct: u8,
    pub stage: String,
}

const MAX_STAGE_CHARS: usize = 80;

/// Strict parser: the LAST JSON object in the reply must carry an integer
/// `progress_pct` in 0..=100 and a non-empty short `stage` string. Anything
/// else (prose, empty output, wrong shapes, oversized stages) is None.
pub fn parse_probe_reply(text: &str) -> Option<ProbeReport> {
    let cand = json_candidates(text).pop()?;
    let v: Value = serde_json::from_str(&cand).ok()?;
    let obj = v.as_object()?;
    let pct = obj.get("progress_pct")?.as_u64()?;
    if pct > 100 {
        return None;
    }
    let stage = obj.get("stage")?.as_str()?.trim();
    if stage.is_empty() || stage.chars().count() > MAX_STAGE_CHARS {
        return None;
    }
    Some(ProbeReport {
        progress_pct: pct as u8,
        stage: stage.to_owned(),
    })
}

/// One probe: a separate short `sprite exec` (its own curl one-shot against
/// the gateway, NOT the lane's harness session). `canned_command` is the
/// test-only override (never from untrusted input); in production the probe
/// command is built from the gateway recipe. Any failure or unusable reply
/// returns None (probes are best-effort).
pub async fn run_probe(
    sprite: &SpriteClient,
    name: &str,
    prompt: &str,
    timeout: Duration,
    connector_id: &str,
    canned_command: Option<&str>,
) -> Option<crate::probe::ProbeReport> {
    let cmd = match canned_command {
        Some(cmd) => cmd.to_owned(),
        None => probe_curl_command(prompt, timeout, connector_id)?,
    };
    let opts = ExecOptions::default()
        .with_timeout(timeout)
        .with_env("OPENCODE_GATEWAY_CONNECTOR", connector_id);
    let out = sprite.exec(name, &cmd, &opts).await.ok()?;
    // Real gateway replies carry the reply inside choices[0].message.content
    // (classified like the curl floor); canned/test replies are the bare
    // JSON object itself, so the raw stdout is the fallback source.
    let text = match classify_gateway_response(&out.stdout) {
        crate::harness::GatewayShape::Text(t) => t,
        _ => out.stdout.clone(),
    };
    parse_probe_reply(&text)
}

/// One-shot (non-loop) curl command against the chat-completions gateway with
/// low reasoning effort — the probe never enters the lane's harness chain.
fn probe_curl_command(prompt: &str, timeout: Duration, connector_id: &str) -> Option<String> {
    let recipe = SpriteRecipe::for_model(format!("probe-{connector_id}"), IMPLEMENTER_MODEL)
        .with_reasoning_effort("low")
        .with_timeout(timeout);
    let request: ChatRequest = recipe.build("", prompt);
    recipe.curl_command(&request).ok()
}

/// Per-lane probe loop: sleep first (probes start after the warmup), then ask
/// on every interval while the lane is still `running` and inside its
/// deadline. A parseable reply is stored as a `progress` lane event and
/// broadcast; anything else is ignored.
pub async fn probe_loop(
    ctx: Arc<PipelineCtx>,
    run_id: String,
    idx: i64,
    sprite_name: String,
    started_ms: i64,
    deadline_ms: i64,
) {
    let interval = ctx.config.probe_interval;
    loop {
        tokio::time::sleep(interval).await;
        let now = now_ms();
        if now >= deadline_ms {
            break;
        }
        let Ok(Some(lane)) = ctx.store.get_lane(&run_id, idx).await else {
            break;
        };
        if lane.state != "running" {
            break;
        }
        let elapsed_min = (now - started_ms).max(0) / 60_000;
        let deadline_in_min = (deadline_ms - now).max(0) / 60_000;
        let prompt = probe_prompt(elapsed_min, deadline_in_min);
        let report = run_probe(
            &ctx.sprite,
            &sprite_name,
            &prompt,
            ctx.config.probe_timeout,
            &ctx.config.gateway_connector_id,
            ctx.config.probe_command.as_deref(),
        )
        .await;
        if let Some(report) = report {
            let payload = serde_json::json!({
                "progress_pct": report.progress_pct,
                "stage": report.stage,
            })
            .to_string();
            if ctx
                .store
                .append_lane_event(&run_id, idx, now_ms(), "progress", &payload)
                .await
                .is_ok()
            {
                publish(&ctx, &run_id).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // test code: panics on failure are the assertion mechanism

    use super::*;

    #[test]
    fn probe_prompt_carries_elapsed_deadline_and_compact_json_request() {
        let p = probe_prompt(12, 48);
        assert!(p.contains("Elapsed: 12m"), "prompt: {p}");
        assert!(p.contains("Deadline in 48m"), "prompt: {p}");
        assert!(
            p.contains("{\"progress_pct\":<0-100>,\"stage\":\"<one short phrase>\"}"),
            "prompt: {p}"
        );
    }

    #[test]
    fn parse_probe_reply_accepts_strict_compact_json() {
        let r = parse_probe_reply("{\"progress_pct\":42,\"stage\":\"building ui\"}").unwrap();
        assert_eq!(r.progress_pct, 42);
        assert_eq!(r.stage, "building ui");
        // chatter around the object is tolerated (the LAST object wins)
        let r =
            parse_probe_reply("noise {\"progress_pct\":7,\"stage\":\"planning\"} tail").unwrap();
        assert_eq!(r.progress_pct, 7);
        // 0 and 100 bounds are valid
        assert_eq!(
            parse_probe_reply("{\"progress_pct\":0,\"stage\":\"s\"}")
                .unwrap()
                .progress_pct,
            0
        );
        assert_eq!(
            parse_probe_reply("{\"progress_pct\":100,\"stage\":\"s\"}")
                .unwrap()
                .progress_pct,
            100
        );
    }

    #[test]
    fn parse_probe_reply_rejects_unusable_replies() {
        assert!(parse_probe_reply("").is_none());
        assert!(parse_probe_reply("no json").is_none());
        assert!(parse_probe_reply("{\"progress_pct\":\"42\",\"stage\":\"s\"}").is_none());
        assert!(parse_probe_reply("{\"progress_pct\":101,\"stage\":\"s\"}").is_none());
        assert!(parse_probe_reply("{\"progress_pct\":-1,\"stage\":\"s\"}").is_none());
        assert!(parse_probe_reply("{\"progress_pct\":42}").is_none());
        assert!(parse_probe_reply("{\"stage\":\"s\"}").is_none());
        assert!(parse_probe_reply("{\"progress_pct\":42,\"stage\":\"\"}").is_none());
        assert!(parse_probe_reply("{\"progress_pct\":42,\"stage\":\"   \"}").is_none());
        // a stage beyond one short phrase is rejected
        let long = "x".repeat(81);
        assert!(
            parse_probe_reply(&format!("{{\"progress_pct\":42,\"stage\":\"{long}\"}}")).is_none()
        );
        // unbalanced JSON is ignored entirely
        assert!(parse_probe_reply("{oops").is_none());
    }
}
