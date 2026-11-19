use llm_gateway::MachineLlmClient;
use prompts::{gate_with_options, parse, GateOptions, JudgeOutput, Role, ValidatorOutput};

pub async fn run_judge(
    client: &MachineLlmClient,
    reports: &[ValidatorOutput],
    extracts: &[String],
) -> Result<(JudgeOutput, f64), String> {
    let prompt = prompts::judge_prompt(reports, extracts);
    // include image parts note: real mode would attach screenshots as image parts;
    // for now text-only prompt with extracts is sufficient and works in tests.
    let request = llm_gateway::ChatRequest::new(llm_gateway::JUDGE_MODEL)
        .system(&prompt.system)
        .user(&prompt.user)
        .temperature(0.2);
    let response = client.chat(&request).await.map_err(|e| e.to_string())?;
    let text = response
        .text()
        .ok_or_else(|| "judge: empty response".to_owned())?;
    let opts = GateOptions {
        expected_variants: None,
        max_verdict_sentences: Some(2),
    };
    gate_with_options(Role::Judge, &text, &opts).map_err(|e| e.to_string())?;
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
    // naive script stripping: remove <script...>...</script> (case-insensitive, non-greedy-ish)
    let mut out = String::with_capacity(html.len().min(max_bytes));
    let lower = html.to_lowercase();
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < bytes.len() {
        if lower[i..].starts_with("<script") {
            // find closing </script>
            if let Some(end_rel) = lower[i..].find("</script>") {
                i += end_rel + "</script>".len();
                continue;
            } else {
                // unclosed script, drop rest
                break;
            }
        }
        // append char
        let ch = html[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
        if out.len() >= max_bytes {
            out.truncate(max_bytes);
            out.push_str("\n...[truncated]");
            break;
        }
    }
    out
}

pub fn validator_reports_from_lanes(lanes: &[store::Lane]) -> Vec<ValidatorOutput> {
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
        reports.push(ValidatorOutput {
            verdict,
            features: prompts::TextOrList::Text(String::new()),
            bugs: prompts::TextOrList::Text(String::new()),
            security: prompts::TextOrList::Text(String::new()),
            quality_score: quality,
            summary,
        });
    }
    reports
}
