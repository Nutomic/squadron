use llm_gateway::MachineLlmClient;
use std::time::Duration;
use tokio::time::timeout;

pub use prompts::LaneJudgeReport;
use prompts::{gate_with_options, parse, GateOptions, JudgeOutput, Role, ValidatorOutput};

pub async fn run_judge(
    client: &MachineLlmClient,
    reports: &[LaneJudgeReport],
    extracts: &[String],
    deadline: Duration,
) -> Result<(JudgeOutput, f64), String> {
    if reports.is_empty() {
        return Err("judge: no validator reports".to_owned());
    }
    if extracts.len() != reports.len() {
        return Err("judge: extracts/reports length mismatch".to_owned());
    }
    let lane_indices: Vec<usize> = reports.iter().map(|r| r.lane_idx).collect();
    let opts = GateOptions {
        expected_variants: None,
        max_verdict_sentences: Some(2),
        expected_lane_indices: Some(lane_indices),
    };
    // The judge future itself owns the request; the deadline is enforced here so
    // an unresponsive judge cannot stall a run forever.
    timeout(deadline, judge_once(client, reports, extracts, &opts))
        .await
        .map_err(|_| "judge: deadline exceeded".to_owned())?
}

async fn judge_once(
    client: &MachineLlmClient,
    reports: &[LaneJudgeReport],
    extracts: &[String],
    opts: &GateOptions,
) -> Result<(JudgeOutput, f64), String> {
    let prompt = prompts::judge_prompt(reports, extracts);
    let request = llm_gateway::ChatRequest::new(llm_gateway::JUDGE_MODEL)
        .system(&prompt.system)
        .user(&prompt.user)
        .temperature(0.2);
    let response = client.chat(&request).await.map_err(|e| e.to_string())?;
    let text = response
        .text()
        .ok_or_else(|| "judge: empty response".to_owned())?;
    gate_with_options(Role::Judge, &text, opts).map_err(|e| e.to_string())?;
    let out: JudgeOutput = parse(Role::Judge, &text).map_err(|e| e.to_string())?;
    let cost = response.cost_usd().unwrap_or(0.0);
    Ok((out, cost))
}

/// Fetch HTML from a URL and sanitize: strip <script> blocks, truncate ~20KB.
pub async fn fetch_sanitized_extract(url: &str) -> String {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    let html = match client.get(url).send().await {
        Ok(r) => r.text().await.unwrap_or_default(),
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
        reports.push(LaneJudgeReport::new(
            l.idx as usize,
            ValidatorOutput {
                verdict,
                features: prompts::TextOrList::Text(String::new()),
                bugs: prompts::TextOrList::Text(String::new()),
                security: prompts::TextOrList::Text(String::new()),
                quality_score: quality,
                summary,
            },
        ));
    }
    reports
}

fn starts_with_ignore_case(haystack: &str, needle: &str) -> bool {
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    hb.len() >= nb.len()
        && hb
            .iter()
            .zip(nb)
            .all(|(h, n)| h.to_ascii_lowercase() == n.to_ascii_lowercase())
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
        let ch = rest.chars().next().unwrap();
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
        if hb[i..i + nb.len()]
            .iter()
            .zip(nb)
            .all(|(h, n)| h.to_ascii_lowercase() == n.to_ascii_lowercase())
        {
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
                                let b = after_name.as_bytes();
                                for j in 2..b.len() {
                                    if b[j] == q {
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
                        // "on" (2) + name (name_len + 1) + "=" (1) + value
                        i += 2 + (name_len + 1) + 1 + value_len;
                        continue;
                    }
                }
            }
        }
        if starts_with_ignore_case(rest, "javascript:") {
            i += "javascript:".len();
            continue;
        }
        let ch = rest.chars().next().unwrap();
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
