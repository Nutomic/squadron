use llm_gateway::MachineLlmClient;
use std::time::Duration;
use tokio::time::{timeout, timeout_at, Instant};

pub use prompts::LaneJudgeReport;
use prompts::{
    gate_and_scan_injections, parse, GateOptions, JudgeOutput, LaneJudgeContext, Role,
    ValidatorOutput,
};

/// Which rung of the judge ladder produced the verdict. Logged into lane
/// events so a run's verdict can be audited without replaying gateway traffic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JudgeRung {
    PrimaryWithImages,
    TextOnly,
    Deterministic,
}

impl JudgeRung {
    pub fn label(self) -> &'static str {
        match self {
            Self::PrimaryWithImages => "glm-5.3-flash+images",
            Self::TextOnly => "deepseek-v4.1-flash text-only",
            Self::Deterministic => "deterministic_fallback",
        }
    }
}

#[derive(Debug)]
pub struct JudgeVerdict {
    pub output: JudgeOutput,
    pub cost_usd: Option<f64>,
    pub rung: JudgeRung,
}

struct JudgeAttempt {
    rung: JudgeRung,
    model: &'static str,
    with_images: bool,
}

pub async fn run_judge(
    client: &MachineLlmClient,
    session: &str,
    brief: &str,
    reports: &[LaneJudgeReport],
    extracts: &[String],
    images: &[Option<String>],
    contexts: &[LaneJudgeContext],
    deadline: Duration,
) -> Result<(JudgeOutput, Option<f64>), String> {
    run_judge_ladder(
        client,
        session,
        brief,
        reports,
        extracts,
        images,
        contexts,
        deadline,
    )
    .await
    .map(|v| (v.output, v.cost_usd))
}

/// F29/F35b: never let one judge request kill a run. Try the pinned flash
/// primary WITH screenshots (image-capable, probe-verified 2026-11-25), then
/// the different-family deepseek text-only rung before reporting failure so
/// the caller can pick a deterministic winner.
pub async fn run_judge_ladder(
    client: &MachineLlmClient,
    session: &str,
    brief: &str,
    reports: &[LaneJudgeReport],
    extracts: &[String],
    images: &[Option<String>],
    contexts: &[LaneJudgeContext],
    deadline: Duration,
) -> Result<JudgeVerdict, String> {
    if reports.is_empty() {
        return Err("judge: no validator reports".to_owned());
    }
    if extracts.len() != reports.len() {
        return Err("judge: extracts/reports length mismatch".to_owned());
    }
    if images.len() != reports.len() {
        return Err("judge: images/reports length mismatch".to_owned());
    }
    if contexts.len() != reports.len() {
        return Err("judge: contexts/reports length mismatch".to_owned());
    }
    let lane_indices: Vec<usize> = reports.iter().map(|r| r.lane_idx).collect();
    let opts = GateOptions {
        expected_variants: None,
        max_verdict_sentences: Some(2),
        expected_lane_indices: Some(lane_indices),
    };
    // Wave A: the judge sees the user's ORIGINAL brief as context; the
    // per-lane rubric is each lane's ASSIGNED CASE (F37), carried in the
    // contexts so the verdict's case_fulfillment measures the winner's case.
    let prompt = prompts::judge_prompt(brief, reports, extracts, contexts);
    let has_images = images.iter().any(|i| i.is_some());
    let per_rung = deadline / 2;
    timeout(deadline, async {
        let mut last_error = "judge: no rung attempted".to_owned();
        let rungs = [
            JudgeAttempt {
                rung: JudgeRung::PrimaryWithImages,
                model: llm_gateway::JUDGE_MODEL,
                with_images: true,
            },
            JudgeAttempt {
                rung: JudgeRung::TextOnly,
                model: llm_gateway::JUDGE_TEXT_FALLBACK_MODEL,
                with_images: false,
            },
        ];
        for attempt in rungs {
            if attempt.with_images && !has_images {
                continue;
            }
            let call = judge_once(client, session, &prompt, reports, images, &opts, &attempt);
            match timeout_at(Instant::now() + per_rung, call).await {
                Ok(Ok((output, cost_usd))) => {
                    return Ok(JudgeVerdict {
                        output,
                        cost_usd,
                        rung: attempt.rung,
                    })
                }
                Ok(Err(e)) => last_error = e,
                Err(_) => last_error = format!("judge: {} deadline exceeded", attempt.rung.label()),
            }
        }
        Err(last_error)
    })
    .await
    .map_err(|_| "judge: deadline exceeded".to_owned())?
}

async fn judge_once(
    client: &MachineLlmClient,
    session: &str,
    prompt: &prompts::Prompt,
    reports: &[LaneJudgeReport],
    images: &[Option<String>],
    opts: &GateOptions,
    attempt: &JudgeAttempt,
) -> Result<(JudgeOutput, Option<f64>), String> {
    // #12: when lane screenshots exist, attach them as image parts so the judge
    // can see the rendered sites, not just the text extracts. Audit R3 finding:
    // images must stay keyed to their lane, never flattened — after one missing
    // render a flattened list would shift every later image onto the wrong
    // lane. Each image is preceded by a per-lane label text part; lanes without
    // a render contribute no image part (they stay text-only in the prompt).
    let request = if attempt.with_images && images.iter().any(|i| i.is_some()) {
        let mut parts = vec![llm_gateway::ContentPart::text(&prompt.user)];
        for (report, img) in reports.iter().zip(images.iter()) {
            if let Some(img) = img {
                parts.push(llm_gateway::ContentPart::text(format!(
                    "lane {}:",
                    report.lane_idx
                )));
                parts.push(llm_gateway::ContentPart::image(img));
            }
        }
        llm_gateway::ChatRequest::new(attempt.model)
            .system(&prompt.system)
            .user_parts(parts)
            .temperature(0.2)
    } else {
        llm_gateway::ChatRequest::new(attempt.model)
            .system(&prompt.system)
            .user(&prompt.user)
            .temperature(0.2)
    };
    // F12: the machine-side request carries a stable x-opencode-session id
    // (opencode-go endpoint requires it; per-run for prompt caching).
    let response = client
        .chat_with_session(&request, session)
        .await
        .map_err(|e| e.to_string())?;
    let text = response
        .text()
        .ok_or_else(|| "judge: empty response".to_owned())?;
    // Security review F36: the judge runs the SAME gate + `[INJECTION?]`
    // marker scan as every other role — a poisoned lane's evidence could
    // otherwise make the judge emit markers that flow into the verdict card.
    // Markers hard-fail the attempt down the existing fallback ladder.
    let (_, markers) =
        gate_and_scan_injections(Role::Judge, &text, opts).map_err(|e| e.to_string())?;
    if !markers.is_empty() {
        return Err(format!(
            "judge: injection markers detected at {}",
            markers.join(",")
        ));
    }
    let out: JudgeOutput = parse(Role::Judge, &text).map_err(|e| e.to_string())?;
    // R3-A #5: unknown judge cost stays None, never a fabricated 0.00.
    Ok((out, response.cost_usd()))
}

/// Fetch HTML from a URL and sanitize: strip <script> blocks, truncate ~20KB.
/// Audit R3 finding: the URL is agent-controlled, so the fetch goes through
/// the shared SSRF-safe fetcher (no unvalidated redirects, no non-public
/// resolved IPs). Any failure degrades to an empty extract.
pub async fn fetch_sanitized_extract(url: &str) -> String {
    let html = match crate::net::fetch_public_text(url).await {
        Ok(html) => html,
        Err(_) => return String::new(),
    };
    sanitize_html(&html, 20_000)
}

pub fn sanitize_html(html: &str, max_bytes: usize) -> String {
    sanitize_html_impl(html, max_bytes)
}

pub fn validator_reports_from_lanes(lanes: &[store::Lane]) -> Vec<LaneJudgeReport> {
    let mut reports = Vec::new();
    for l in lanes {
        if l.state != "validated" && l.val_verdict.is_none() {
            continue;
        }
        let verdict = match l.val_verdict.as_deref() {
            Some("pass") => prompts::Verdict::Pass,
            _ => prompts::Verdict::Fail,
        };
        let summary = l.val_summary.clone().unwrap_or_default();
        let quality = l.val_quality.unwrap_or(0.0);
        // Hydrate the full report (features/bugs/security) from the persisted
        // validator output when available; otherwise empty placeholders. The
        // reader tolerates legacy string-shaped reports and drops
        // placeholder/empty entries (F18).
        let out = l
            .val_report_json
            .as_deref()
            .and_then(prompts::parse_validator_report)
            .unwrap_or(ValidatorOutput {
                verdict,
                features: Vec::new(),
                bugs: Vec::new(),
                security: Vec::new(),
                quality_score: quality,
                summary,
            });
        reports.push(LaneJudgeReport::new(l.idx as usize, out));
    }
    reports
}

/// F37: the lane's assigned case, parsed from the persisted variant_json.
/// Decomposition cases carry `{label, impl_prompt}` (the same shape the
/// confirm dialog persists); legacy template variants carry
/// `{angle, stack, ...}` — the angle is the closest thing those lanes had
/// to a case contract. Unparsable JSON yields no case (the judge's section
/// then says so explicitly).
pub fn lane_case_from_variant_json(variant_json: &str) -> (Option<String>, Option<String>) {
    let Ok(variant) = serde_json::from_str::<serde_json::Value>(variant_json) else {
        return (None, None);
    };
    if let Some(case) = variant.get("impl_prompt").and_then(|v| v.as_str()) {
        let label = variant
            .get("label")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        return (label, Some(case.to_owned()));
    }
    if let Some(angle) = variant.get("angle").and_then(|v| v.as_str()) {
        let stack = variant.get("stack").and_then(|v| v.as_str());
        let case = match stack {
            Some(stack) => format!("Angle: {angle} (stack: {stack})"),
            None => format!("Angle: {angle}"),
        };
        return (Some(angle.to_owned()), Some(case));
    }
    (None, None)
}

/// F37: format the judge-time fresh live-check line for one surviving lane.
/// A 2xx probe yields "live: 200 (rechecked HH:MM)" (UTC); anything else —
/// non-200 status or transport failure — yields the unreachable form so the
/// judge cannot mistake a stale screenshot frame for the current state.
pub async fn live_check_text(
    url: Option<&str>,
    allow_loopback: bool,
) -> String {
    let Some(url) = url else {
        return "live: unreachable at judge time (no public URL)".to_owned();
    };
    match crate::net::fetch_public_status(url, allow_loopback).await {
        Ok(status) if (200..300).contains(&status) => {
            format!("live: {status} (rechecked {})", utc_hhmm(store::now_ms()))
        }
        Ok(status) => format!("live: unreachable at judge time (HTTP {status})"),
        Err(_) => "live: unreachable at judge time".to_owned(),
    }
}

/// UTC HH:MM for a fresh-check timestamp, derived from epoch ms without a
/// calendar dependency (judge-time evidence only needs minute resolution).
pub fn utc_hhmm(epoch_ms: i64) -> String {
    let secs = epoch_ms.div_euclid(1000);
    let hour = secs.div_euclid(3600).rem_euclid(24);
    let minute = secs.div_euclid(60).rem_euclid(60);
    format!("{hour:02}:{minute:02}")
}

fn starts_with_ignore_case(haystack: &str, needle: &str) -> bool {
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    hb.len() >= nb.len() && hb[..nb.len()].eq_ignore_ascii_case(nb)
}

fn sanitize_html_impl(html: &str, max_bytes: usize) -> String {
    // Pass 1: remove dangerous element blocks, scanning the ORIGINAL string so
    // multi-byte characters never shift offsets (case-insensitive by ASCII).
    let mut stripped = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // Work char-wise so every append/skip lands on a char boundary.
        let rest = &html[i..];
        let lower_start = |tag: &str| {
            starts_with_ignore_case(rest, tag)
                && rest
                    .as_bytes()
                    .get(tag.len())
                    .map(|b| {
                        b.is_ascii_whitespace()
                            || *b == b'>'
                            || *b == b'/'
                            || !b.is_ascii_alphanumeric()
                    })
                    .unwrap_or(true)
        };
        if rest.starts_with('<')
            && (lower_start("<script") || lower_start("<iframe") || lower_start("<style"))
        {
            let tag_end = match find_tag_end(rest) {
                Some(pos) => pos,
                None => break, // unterminated open tag: drop remainder
            };
            let tag_name: String = rest[1..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            let close = find_case_insensitive(&rest[tag_end..], &format!("</{}", tag_name));
            match close {
                Some(rel) => {
                    // advance past "</tag ... >"
                    let close_abs = tag_end + rel;
                    let after_close = match find_tag_end(&rest[close_abs..]) {
                        Some(p) => close_abs + p,
                        None => rest.len(),
                    };
                    i += after_close;
                    continue;
                }
                None => break, // unclosed dangerous element: drop rest
            }
        }
        if rest.starts_with('<') && (lower_start("<meta") || lower_start("<base")) {
            match find_tag_end(rest) {
                Some(pos) => {
                    i += pos;
                    continue;
                }
                None => break,
            }
        }
        let Some(ch) = rest.chars().next() else {
            break;
        };
        stripped.push(ch);
        i += ch.len_utf8();
    }

    // Pass 2: neutralize on* handlers and javascript: URLs (case-insensitive).
    let stripped = strip_event_handlers_and_js(&stripped);

    // Pass 3: truncate at a char boundary.
    truncate_chars(&stripped, max_bytes)
}

/// Find the `>` closing a tag, skipping quoted attribute values.
/// Returns the offset just past `>`.
fn find_tag_end(tag: &str) -> Option<usize> {
    let bytes = tag.as_bytes();
    let mut quote: Option<u8> = None;
    for (i, b) in bytes.iter().enumerate() {
        match quote {
            Some(q) if *b == q => quote = None,
            Some(_) => {}
            None => match b {
                b'"' | b'\'' => quote = Some(*b),
                b'>' => return Some(i + 1),
                _ => {}
            },
        }
    }
    None
}

fn find_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    for i in 0..=(hb.len() - nb.len()) {
        if hb[i..i + nb.len()].eq_ignore_ascii_case(nb) {
            return Some(i);
        }
    }
    None
}

fn strip_event_handlers_and_js(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < input.len() {
        let rest = &input[i..];
        // on<attr>= handler, only at an attribute position (after whitespace or '<')
        let at_boundary = i == 0
            || input
                .as_bytes()
                .get(i - 1)
                .map(|b| b.is_ascii_whitespace() || *b == b'<' || *b == b'/')
                .unwrap_or(false);
        if at_boundary && starts_with_ignore_case(rest, "on") {
            let after_on = &rest[2..];
            let name_len = after_on
                .char_indices()
                .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '-')
                .map(|(idx, _)| idx)
                .last();
            if let Some(name_len) = name_len {
                if name_len > 0 {
                    let after_name = &after_on[name_len + 1..]; // byte after the attribute name
                    if after_name.starts_with('=') {
                        // remove the whole attribute (on<name>=value)
                        let value_len = match after_name.as_bytes().get(1) {
                            Some(b'"') | Some(b'\'') => {
                                let q = after_name.as_bytes()[1];
                                let mut end = None;
                                for (j, c) in after_name.as_bytes().iter().enumerate().skip(2) {
                                    if *c == q {
                                        end = Some(j + 1);
                                        break;
                                    }
                                }
                                end.unwrap_or(after_name.len())
                            }
                            _ => after_name
                                .find(|c: char| c.is_ascii_whitespace() || c == '>')
                                .unwrap_or(after_name.len()),
                        };
                        // "on" (2) + name (name_len + 1) + rest of after_name
                        // (value_len counts from '=' inclusive: adding a
                        // separate '=' term would double-count it and eat the
                        // closing '>'.
                        i += 2 + (name_len + 1) + value_len;
                        continue;
                    }
                }
            }
        }
        if starts_with_ignore_case(rest, "javascript:") {
            i += "javascript:".len();
            continue;
        }
        let Some(ch) = rest.chars().next() else {
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn truncate_chars(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut out = String::with_capacity(max_bytes + 16);
    let mut len = 0usize;
    for ch in s.chars() {
        let cl = ch.len_utf8();
        if len + cl > max_bytes {
            break;
        }
        out.push(ch);
        len += cl;
    }
    out.push_str("\n...[truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F37: the lane's assigned case is parsed from the persisted
    /// variant_json — decomposition cases carry {label, impl_prompt};
    /// legacy template variants carry the angle as the closest case
    /// contract; garbage yields no case.
    #[test]
    fn lane_case_from_variant_json_shapes() {
        // Decomposition case (the shape the confirm dialog persists).
        let (label, case) = lane_case_from_variant_json(
            r#"{"label":"navy palette","impl_prompt":"Build a Netscape-era 1996 homepage. Angle: navy and steel blue palette."}"#,
        );
        assert_eq!(label.as_deref(), Some("navy palette"));
        assert!(
            case.as_deref().unwrap_or_default().contains("navy and steel blue"),
            "case: {case:?}"
        );
        // Legacy template variant: the angle stands in for the case.
        let (label, case) =
            lane_case_from_variant_json(r#"{"angle":"retro arcade","stack":"vanilla js"}"#);
        assert_eq!(label.as_deref(), Some("retro arcade"));
        assert!(
            case.as_deref().unwrap_or_default().contains("Angle: retro arcade"),
            "case: {case:?}"
        );
        // Unparsable / case-less JSON yields nothing.
        assert_eq!(lane_case_from_variant_json("not json"), (None, None));
        assert_eq!(lane_case_from_variant_json("{}"), (None, None));
    }

    /// F37: the fresh-check timestamp renders as UTC HH:MM.
    #[test]
    fn utc_hhmm_renders_minutes() {
        assert_eq!(utc_hhmm(0), "00:00");
        // 1h 1m after the epoch (UTC).
        assert_eq!(utc_hhmm((3600 + 60) * 1000), "01:01");
        // Day wrap: 23:59 then midnight.
        assert_eq!(utc_hhmm(23 * 3600_000 + 59 * 60_000), "23:59");
        assert_eq!(utc_hhmm(24 * 3600_000), "00:00");
        // Negative timestamps (defensive) stay on [0,24).
        assert_eq!(utc_hhmm(-60_000), "23:59");
    }

    /// #25: an unclosed <script> must be dropped, not passed through.
    #[test]
    fn sanitize_drops_unclosed_script() {
        let html = "<p>ok</p><script>alert(1)";
        let out = sanitize_html(html, 20_000);
        assert!(!out.contains("script"), "out={out}");
        assert!(!out.contains("alert(1)"), "out={out}");
        assert!(out.contains("<p>ok</p>"));
    }

    /// #25: `</script >` (whitespace before `>`) still closes the block.
    #[test]
    fn sanitize_handles_close_tag_with_trailing_space() {
        let html = "<script>x</script >after";
        let out = sanitize_html(html, 20_000);
        assert!(!out.contains("<script"), "out={out}");
        assert!(out.contains("after"), "out={out}");
    }

    /// #25: multi-byte content plus truncation must never panic or split a char.
    #[test]
    fn sanitize_multibyte_truncation_no_panic() {
        let html = format!("<p>İllegal 😀 {}</p><script>bad</script>", "é".repeat(5000));
        let out = sanitize_html(&html, 512);
        assert!(out.len() <= 512 + "...[truncated]".len());
        assert!(out.is_char_boundary(out.len()), "out={out:?}");
        assert!(!out.contains("bad"));
    }

    /// #25: on* handlers and javascript: URLs are neutralized.
    #[test]
    fn sanitize_strips_event_handlers_and_js_urls() {
        let html = r#"<a href="javascript:alert(1)" onclick="evil()">c</a>"#;
        let out = sanitize_html(html, 20_000);
        assert!(!out.contains("javascript:"), "out={out}");
        assert!(!out.contains("onclick"), "out={out}");
        assert!(out.contains(">c</a>") || out.contains("c"), "out={out}");
    }
}
