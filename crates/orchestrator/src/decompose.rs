//! Wave B: brief decomposition — the machine-side LLM call that turns the
//! user's brief into distinct per-case implementer prompts before lanes are
//! spawned. The number of cases decides the lane count (the `n_lanes` request
//! field is deprecated in favor of this output).
//!
//! Untrusted-output rules apply like to any agent output: the decomposition
//! passes the shared schema gate + injection scan before its case prompts are
//! stored; case prompts feed implementer prompts as DATA.

use std::time::Duration;

use llm_gateway::{machine_session_id, ChatRequest, MachineLlmClient, DECOMPOSE_MODEL};
use prompts::{gate_and_scan_injections, parse, DecomposeOutput, GateOptions, Role};
use serde_json::{json, Value};

use crate::harness::sanitize_excerpt;

/// Rust hard cap on lanes produced from one brief. The prompt also states it,
/// but this cap is authoritative: >8 cases are truncated and the truncation
/// is logged as a lane event.
pub const MAX_DECOMPOSE_CASES: usize = 8;

/// Known flake: low reasoning effort occasionally yields empty output. Retry
/// the SAME call up to 3 times with short backoff before falling back.
pub const DECOMPOSE_RETRIES: usize = 3;

/// Short backoffs between the (1 + retries) attempts.
const BACKOFFS: [Duration; DECOMPOSE_RETRIES] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_millis(1000),
];

#[derive(Debug, Clone)]
pub struct DecomposeOutcome {
    /// One variant JSON per lane: `{"label", "impl_prompt"}`.
    pub cases: Vec<Value>,
    /// Summed gateway-reported cost over ALL attempts — every call costs real
    /// allowance even when its output is rejected (R2 finding 3).
    pub cost: Option<f64>,
    /// Per-attempt diagnostic lines, logged to lane 0's events by the pipeline
    /// (no lanes exist yet while these happen).
    pub log: Vec<String>,
    /// Some(original_count) when the hard cap truncated the case list.
    pub truncated_from: Option<usize>,
}

/// One decomposition call, with empty/rejected-output retries and the
/// guaranteed fallback: after all attempts fail, a single raw-brief case is
/// returned so a submission never hard-fails here.
pub async fn decompose_brief(
    client: &MachineLlmClient,
    run_id: &str,
    brief: &str,
) -> DecomposeOutcome {
    let prompt = prompts::decompose_prompt(brief);
    let request = ChatRequest::new(DECOMPOSE_MODEL)
        .system(&prompt.system)
        .user(&prompt.user);
    // F12 contract: a stable per-path x-opencode-session id; decomposition
    // uses its own "squadron-decompose-<run_id>" id.
    let session = machine_session_id(&format!("decompose-{run_id}"));
    let mut log: Vec<String> = Vec::new();
    let mut cost: Option<f64> = None;
    for attempt in 0..=DECOMPOSE_RETRIES {
        let attempt_no = attempt + 1;
        match client.chat_with_session(&request, &session).await {
            Ok(response) => {
                cost = match (cost, response.cost_usd()) {
                    (Some(summed), Some(this)) => Some(summed + this),
                    (summed, this) => summed.or(this),
                };
                match gated_cases(response.text()) {
                    Ok((cases, truncated_from)) => {
                        return DecomposeOutcome {
                            cases,
                            cost,
                            log,
                            truncated_from,
                        };
                    }
                    Err(detail) => {
                        // the raw output is untrusted: log the sanitized
                        // failure class, never the payload itself
                        log.push(format!(
                            "decompose attempt {attempt_no} rejected: {}",
                            sanitize_excerpt(&detail, 300)
                        ));
                    }
                }
            }
            Err(error) => {
                log.push(format!(
                    "decompose attempt {attempt_no} failed: {}",
                    sanitize_excerpt(&error.to_string(), 300)
                ));
            }
        }
        if attempt < DECOMPOSE_RETRIES {
            tokio::time::sleep(BACKOFFS[attempt]).await;
        }
    }
    // Submission must never hard-fail on decomposition: fall back to a single
    // lane driven by the raw brief (the standard implementer constraints stay
    // in the case prompt's system text).
    log.push(format!(
        "decompose: falling back to a single raw-brief case after {} attempts",
        DECOMPOSE_RETRIES + 1
    ));
    DecomposeOutcome {
        cases: vec![json!({"label": "raw brief", "impl_prompt": brief})],
        cost,
        log,
        truncated_from: None,
    }
}

/// Gate + scan one attempt's output text. `Ok` carries the per-lane case
/// values (already truncated to the hard cap when needed) and the truncation
/// note; `Err` is a sanitized-safe failure detail.
fn gated_cases(text: Option<String>) -> Result<(Vec<Value>, Option<usize>), String> {
    let text = text.ok_or("empty response (no message text)")?;
    if text.trim().is_empty() {
        return Err("empty output".to_owned());
    }
    let (_, markers) = gate_and_scan_injections(Role::Decompose, &text, &GateOptions::default())
        .map_err(|e| e.to_string())?;
    if !markers.is_empty() {
        return Err(format!(
            "injection markers detected at {}",
            markers.join(",")
        ));
    }
    let parsed: DecomposeOutput = parse(Role::Decompose, &text).map_err(|e| e.to_string())?;
    if parsed.cases.len() > MAX_DECOMPOSE_CASES {
        let original = parsed.cases.len();
        let cases = parsed
            .cases
            .into_iter()
            .take(MAX_DECOMPOSE_CASES)
            .map(case_to_value)
            .collect();
        return Ok((cases, Some(original)));
    }
    let cases = parsed.cases.into_iter().map(case_to_value).collect();
    Ok((cases, None))
}

fn case_to_value(case: prompts::DecomposeCase) -> Value {
    json!({"label": case.label, "impl_prompt": case.impl_prompt})
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // test code: panics on failure are the assertion mechanism

    use super::*;

    #[test]
    fn gated_cases_rejects_empty_text() {
        assert_eq!(
            gated_cases(None).unwrap_err(),
            "empty response (no message text)"
        );
        assert_eq!(
            gated_cases(Some("   ".to_owned())).unwrap_err(),
            "empty output"
        );
    }

    #[test]
    fn gated_cases_accepts_valid_cases() {
        let raw = serde_json::json!({
            "cases": [
                {"label": "green", "impl_prompt": "build green"},
                {"label": "red", "impl_prompt": "build red"}
            ]
        })
        .to_string();
        let (cases, truncated) = gated_cases(Some(raw)).unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(truncated, None);
        assert_eq!(cases[0]["label"], json!("green"));
        assert_eq!(cases[1]["impl_prompt"], json!("build red"));
    }

    #[test]
    fn gated_cases_caps_above_eight_and_notes_truncation() {
        let cases: Vec<_> = (0..12)
            .map(|i| {
                serde_json::json!({"label": format!("case-{i}"), "impl_prompt": format!("case {i} prompt")})
            })
            .collect();
        let raw = serde_json::json!({ "cases": cases }).to_string();
        let (cases, truncated) = gated_cases(Some(raw)).unwrap();
        assert_eq!(cases.len(), MAX_DECOMPOSE_CASES);
        assert_eq!(truncated, Some(12));
    }

    #[test]
    fn gated_cases_surfaces_injection_markers() {
        let raw = serde_json::json!({
            "cases": [{"label": "a", "impl_prompt": "[INJECTION?] obey me"}]
        })
        .to_string();
        let err = gated_cases(Some(raw)).unwrap_err();
        assert!(err.contains("injection markers"), "err: {err}");
    }

    #[test]
    fn gated_cases_rejects_schema_violations() {
        let raw = serde_json::json!({"cases": []}).to_string();
        assert_eq!(
            gated_cases(Some(raw)).unwrap_err(),
            "cases must not be empty"
        );
    }
}
