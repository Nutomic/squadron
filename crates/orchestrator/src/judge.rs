use llm_gateway::MachineLlmClient;
use std::time::Duration;
use tokio::time::timeout;

pub use prompts::LaneJudgeReport;
use prompts::{gate_with_options, parse, GateOptions, JudgeOutput, Role, ValidatorOutput};

pub async fn run_judge(
    client: &MachineLlmClient,
    session: &str,
    reports: &[LaneJudgeReport],
    extracts: &[String],
    images: &[Option<String>],
    deadline: Duration,
) -> Result<(JudgeOutput, Option<f64>), String> {
    if reports.is_empty() {
        return Err("judge: no validator reports".to_owned());
    }
    if extracts.len() != reports.len() {
        return Err("judge: extracts/reports length mismatch".to_owned());
    }
    if images.len() != reports.len() {
        return Err("judge: images/reports length mismatch".to_owned());
    }
    let lane_indices: Vec<usize> = reports.iter().map(|r| r.lane_idx).collect();
    let opts = GateOptions {
        expected_variants: None,
        max_verdict_sentences: Some(2),
        expected_lane_indices: Some(lane_indices),
    };
    // The judge future itself owns the request; the deadline is enforced here so
    // an unresponsive judge cannot stall a run forever.
    timeout(
        deadline,
        judge_once(client, session, reports, extracts, images, &opts),
    )
    .await
    .map_err(|_| "judge: deadline exceeded".to_owned())?
}

async fn judge_once(
    client: &MachineLlmClient,
    session: &str,
    reports: &[LaneJudgeReport],
    extracts: &[String],
    images: &[Option<String>],
    opts: &GateOptions,
) -> Result<(JudgeOutput, Option<f64>), String> {
    let prompt = prompts::judge_prompt(reports, extracts);
    // #12: when lane screenshots exist, attach them as image parts so the judge
    // can see the rendered sites, not just the text extracts. Audit R3 finding:
    // images must stay keyed to their lane, never flattened — after one missing
    // render a flattened list would shift every later image onto the wrong
    // lane. Each image is preceded by a per-lane label text part; lanes without
    // a render contribute no image part (they stay text-only in the prompt).
    let request = if images.iter().any(|i| i.is_some()) {
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
        llm_gateway::ChatRequest::new(llm_gateway::JUDGE_MODEL)
            .system(&prompt.system)
            .user_parts(parts)
            .temperature(0.2)
    } else {
        llm_gateway::ChatRequest::new(llm_gateway::JUDGE_MODEL)
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
    gate_with_options(Role::Judge, &text, opts).map_err(|e| e.to_string())?;
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
        // validator output when available; otherwise empty placeholders.
        let out = l
            .val_report_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<ValidatorOutput>(json).ok())
            .unwrap_or(ValidatorOutput {
                verdict,
                features: prompts::TextOrList::Text(String::new()),
                bugs: prompts::TextOrList::Text(String::new()),
                security: prompts::TextOrList::Text(String::new()),
                quality_score: quality,
                summary,
            });
        reports.push(LaneJudgeReport::new(l.idx as usize, out));
    }
    reports
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
