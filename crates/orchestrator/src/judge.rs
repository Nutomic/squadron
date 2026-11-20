use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use llm_gateway::{
    ChatMessage, ChatRequest, ContentPart, MachineLlmClient, MessageContent, JUDGE_MODEL,
};
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
    let screenshots = vec![None; reports.len()];
    run_judge_with_deadline(client, reports, extracts, &screenshots, deadline).await
}

/// Judge invocation under an enforced deadline (#7): when `deadline` elapses the
/// error fails the run instead of letting an unresponsive judge stall it.
///
/// `screenshots` (#12) carries one optional spec per lane, aligned with
/// `reports`: an `http(s)://` URL or a local PNG file path. When present, the
/// PNG is attached to the judge request as an image content part.
pub async fn run_judge_with_deadline(
    client: &MachineLlmClient,
    reports: &[LaneJudgeReport],
    extracts: &[String],
    screenshots: &[Option<String>],
    deadline: Duration,
) -> Result<(JudgeOutput, f64), String> {
    if reports.is_empty() {
        return Err("judge: no validator reports".to_owned());
    }
    if extracts.len() != reports.len() {
        return Err("judge: extracts/reports length mismatch".to_owned());
    }
    if screenshots.len() != reports.len() {
        return Err("judge: screenshots/reports length mismatch".to_owned());
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
        judge_once(client, reports, extracts, screenshots, &opts),
    )
    .await
    .map_err(|_| "judge: deadline exceeded".to_owned())?
}

fn build_judge_request(
    reports: &[LaneJudgeReport],
    extracts: &[String],
    screenshots: &[Option<String>],
) -> ChatRequest {
    let prompt = prompts::judge_prompt(reports, extracts);
    let mut request = ChatRequest::new(JUDGE_MODEL)
        .system(&prompt.system)
        .temperature(0.2);
    request = match judge_user_message(&prompt.user, screenshots) {
        MessageContent::Text(text) => request.user(text),
        parts => request.message(ChatMessage::new("user", parts)),
    };
    request
}

/// #12: the judge message is text plus one image part per lane screenshot.
fn judge_user_message(user_text: &str, screenshots: &[Option<String>]) -> MessageContent {
    let mut parts: Vec<ContentPart> = Vec::new();
    parts.push(ContentPart::text(user_text.to_owned()));
    for (lane_idx, spec) in screenshots.iter().enumerate() {
        let Some(spec) = spec else { continue };
        let Some(part) = screenshot_image_part(spec) else {
            continue;
        };
        parts.push(ContentPart::text(format!("screenshot of lane {lane_idx}:")));
        parts.push(part);
    }
    if parts.len() == 1 {
        MessageContent::Text(user_text.to_owned())
    } else {
        MessageContent::Parts(parts)
    }
}

/// Resolve a screenshot spec (URL or local PNG path) to an image content part.
fn screenshot_image_part(spec: &str) -> Option<ContentPart> {
    if spec.starts_with("http://") || spec.starts_with("https://") {
        return Some(ContentPart::image(spec));
    }
    let bytes = std::fs::read(spec).ok()?;
    let data_uri = format!("data:image/png;base64,{}", B64.encode(bytes));
    Some(ContentPart::image(data_uri))
}

async fn judge_once(
    client: &MachineLlmClient,
    reports: &[LaneJudgeReport],
    extracts: &[String],
    screenshots: &[Option<String>],
    opts: &GateOptions,
) -> Result<(JudgeOutput, f64), String> {
    let request = build_judge_request(reports, extracts, screenshots);
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
        reports.push(LaneJudgeReport::new(
            l.idx as usize,
            fallback_validator_output(l),
        ));
    }
    reports
}

/// Legacy fallback built from the coarse per-lane columns.
fn fallback_validator_output(l: &store::Lane) -> ValidatorOutput {
    let verdict = match l.val_verdict.as_deref() {
        Some("pass") => prompts::Verdict::Pass,
        _ => prompts::Verdict::Fail,
    };
    ValidatorOutput {
        verdict,
        features: prompts::TextOrList::Text(String::new()),
        bugs: prompts::TextOrList::Text(String::new()),
        security: prompts::TextOrList::Text(String::new()),
        quality_score: l.val_quality.unwrap_or(0.0),
        summary: l.val_summary.clone().unwrap_or_default(),
    }
}

/// #18: parse a persisted full validator report back into `ValidatorOutput`.
pub fn validator_output_from_report_json(raw: &str) -> Option<ValidatorOutput> {
    serde_json::from_str(raw).ok()
}

/// #18: build judge reports hydrating the FULL persisted validator report
/// (features/bugs/security) when available, falling back to the coarse
/// per-lane columns for lanes without a persisted report.
pub async fn validator_reports_from_store(
    store: &store::Store,
    lanes: &[store::Lane],
) -> Result<Vec<LaneJudgeReport>, String> {
    let run_id = lanes.first().map(|l| l.run_id.clone()).unwrap_or_default();
    let persisted = store
        .val_reports_for_run(&run_id)
        .await
        .map_err(|e| e.to_string())?;
    let mut reports = Vec::new();
    for l in lanes {
        if l.state != "validated" && l.val_verdict.is_none() {
            continue;
        }
        let report = persisted
            .get(&l.idx)
            .and_then(|raw| validator_output_from_report_json(raw))
            .unwrap_or_else(|| fallback_validator_output(l));
        reports.push(LaneJudgeReport::new(l.idx as usize, report));
    }
    Ok(reports)
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
                        // "on" (2) + name (name_len + 1) + rest of after_name
                        // (value_len counts from '=' inclusive: value alone
                        // would double-count the '=' and eat the '>').
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

#[cfg(test)]
mod tests {
    use super::*;
    use llm_gateway::MessageContent;
    use prompts::{TextOrList, Verdict};
    use std::io::Write;

    fn sample_reports() -> Vec<LaneJudgeReport> {
        vec![LaneJudgeReport::new(
            0,
            ValidatorOutput {
                verdict: Verdict::Pass,
                features: TextOrList::Text("loop".into()),
                bugs: TextOrList::Text(String::new()),
                security: TextOrList::Text("clean".into()),
                quality_score: 80.0,
                summary: "ok".into(),
            },
        )]
    }

    #[tokio::test]
    async fn judge_deadline_fails_the_run_when_elapsed() {
        // Server that accepts connections but never answers: the judge future
        // hangs and only the deadline can end the run.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let keeper = tokio::spawn(async move {
            let l = tokio::net::TcpListener::from_std(listener).unwrap();
            while let Ok((sock, _)) = l.accept().await {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    drop(sock);
                });
            }
        });
        let client =
            MachineLlmClient::new("test-key", format!("http://{addr}/v1/chat/completions"))
                .with_timeout(Duration::from_secs(30));
        let reports = sample_reports();
        let extracts = vec!["<p>site</p>".to_owned()];
        let shots = vec![None];
        let result = run_judge_with_deadline(
            &client,
            &reports,
            &extracts,
            &shots,
            Duration::from_millis(150),
        )
        .await;
        keeper.abort();
        assert_eq!(result.unwrap_err(), "judge: deadline exceeded");
    }

    #[test]
    fn judge_request_attaches_png_image_parts_per_lane() {
        let dir = tempfile::tempdir().unwrap();
        let png_path = dir.path().join("lane-0.png");
        let mut f = std::fs::File::create(&png_path).unwrap();
        f.write_all(&[0x89, b'P', b'N', b'G', 0, 1, 2, 3]).unwrap();
        drop(f);

        let reports = sample_reports();
        let extracts = vec!["<p>site</p>".to_owned()];

        // no screenshots: plain text message
        let plain = build_judge_request(&reports, &extracts, &[None]);
        let last = plain.messages.last().unwrap();
        assert!(matches!(last.content, MessageContent::Text(_)));

        // local PNG path: image part with data URI
        let request = build_judge_request(
            &reports,
            &extracts,
            &[Some(png_path.to_string_lossy().into_owned())],
        );
        let last = request.messages.last().unwrap();
        let parts = match &last.content {
            MessageContent::Parts(parts) => parts,
            other => panic!("expected parts, got {other:?}"),
        };
        let images: Vec<&ContentPart> = parts.iter().filter(|p| p.kind == "image_url").collect();
        assert_eq!(images.len(), 1);
        assert!(images[0]
            .image_url
            .as_ref()
            .unwrap()
            .url
            .starts_with("data:image/png;base64,"));

        // missing file: no image part, falls back to text-only
        let missing = build_judge_request(&reports, &extracts, &[Some("/nonexistent.png".into())]);
        assert!(matches!(
            missing.messages.last().unwrap().content,
            MessageContent::Text(_)
        ));

        // http spec passes straight through as image URL
        let http = build_judge_request(&reports, &extracts, &[Some("https://x/y.png".into())]);
        let parts = match &http.messages.last().unwrap().content {
            MessageContent::Parts(parts) => parts,
            other => panic!("expected parts, got {other:?}"),
        };
        assert!(parts
            .iter()
            .any(|p| p.image_url.as_ref().map(|i| i.url.as_str()) == Some("https://x/y.png")));
    }

    #[test]
    fn validator_output_hydrates_from_persisted_report_json() {
        let raw = serde_json::json!({
            "verdict": "pass",
            "features": ["game loop", "scoreboard"],
            "bugs": ["flicker on level 2"],
            "security": ["no secrets"],
            "quality_score": 88.5,
            "summary": "detailed"
        })
        .to_string();
        let out = validator_output_from_report_json(&raw).unwrap();
        assert_eq!(out.features.text(), "game loop\nscoreboard");
        assert_eq!(out.bugs.text(), "flicker on level 2");
        assert_eq!(out.security.text(), "no secrets");
        assert_eq!(out.quality_score, 88.5);
        assert!(validator_output_from_report_json("not json").is_none());
    }

    #[tokio::test]
    async fn validator_reports_hydrate_from_store_then_fall_back() {
        let store = store::Store::connect_in_memory().await.unwrap();
        store.migrate().await.unwrap();
        let run = store.create_run("brief", 2, 100).await.unwrap();
        store.insert_lane(&run.id, 0, "{}").await.unwrap();
        store.insert_lane(&run.id, 1, "{}").await.unwrap();
        store
            .finish_validation(&run.id, 0, "pass", "summary0", 70.0)
            .await
            .unwrap();
        store
            .finish_validation(&run.id, 1, "fail", "summary1", 40.0)
            .await
            .unwrap();
        let full = serde_json::json!({
            "verdict": "pass",
            "features": ["f1", "f2"],
            "bugs": "b1",
            "security": "s1",
            "quality_score": 95.0,
            "summary": "hydrated report"
        })
        .to_string();
        store.set_val_report(&run.id, 0, &full, 500).await.unwrap();

        let lanes = store.list_lanes(&run.id).await.unwrap();
        let reports = validator_reports_from_store(&store, &lanes).await.unwrap();
        assert_eq!(reports.len(), 2);
        let hydrated = &reports[0].report;
        assert_eq!(hydrated.features.text(), "f1\nf2");
        assert_eq!(hydrated.summary, "hydrated report");
        assert_eq!(hydrated.quality_score, 95.0);
        let fallback = &reports[1].report;
        assert_eq!(fallback.features.text(), "");
        assert_eq!(fallback.summary, "summary1");
        assert_eq!(fallback.quality_score, 40.0);
        assert_eq!(fallback.verdict, Verdict::Fail);
    }

    // --- sanitize_html: #15 vectors and #25 table-driven tests ---

    #[test]
    fn sanitize_never_panics_on_length_changing_to_lowercase() {
        // İ lowercases to a 2-char sequence; the scanner must not shift offsets.
        let out = sanitize_html("İİ<ScRiPt>alert(1)</ScRiPt>ok", 1000);
        assert_eq!(out, "İİok");
    }

    #[test]
    fn sanitize_table_driven_vectors() {
        struct Case {
            name: &'static str,
            input: &'static str,
            banned: &'static [&'static str],
            kept: &'static [&'static str],
        }
        let cases = [
            Case {
                name: "unclosed script drops remainder",
                input: "<p>safe</p><script>alert(1)",
                banned: &["alert", "<script"],
                kept: &["<p>safe</p>"],
            },
            Case {
                name: "close tag with trailing space",
                input: "<script>evil()</script >tail",
                banned: &["evil", "<script"],
                kept: &["tail"],
            },
            Case {
                name: "non-ascii survives",
                input: "İ<h1>Başlık</h1>",
                banned: &[],
                kept: &["İ", "Başlık"],
            },
            Case {
                name: "mixed-case script",
                input: "<ScRiPt>x</sCrIpT>after",
                banned: &["<script", "</script"],
                kept: &["after"],
            },
            Case {
                name: "on* handlers stripped",
                input: "<img src=x OnErRoR=alert(1)> <div onclick=\"evil()\">hi</div>",
                banned: &["onerror", "onclick", "alert", "evil"],
                kept: &["<img src=x ", "> <div ", ">hi</div>"],
            },
            Case {
                name: "javascript urls stripped",
                // scheme removal defuses the vector; the residual token is an
                // inert relative URL, never a script scheme
                input: "<a href=\"JaVaScRiPt:alert(1)\">x</a>",
                banned: &["javascript"],
                kept: &["<a href=\"", "\">x</a>"],
            },
            Case {
                name: "iframe stripped",
                input: "<iframe src=\"https://evil\"></iframe>keep",
                banned: &["<iframe", "evil\""],
                kept: &["keep"],
            },
            Case {
                name: "style stripped",
                input: "<style>body{background:url(evil)}</style>text",
                banned: &["<style", "background"],
                kept: &["text"],
            },
            Case {
                name: "base stripped",
                input: "<base href=\"https://evil/\"><p>ok</p>",
                banned: &["<base", "href"],
                kept: &["<p>ok</p>"],
            },
            Case {
                name: "meta http-equiv refresh stripped",
                input: "<META HTTP-EQUIV=\"refresh\" CONTENT=\"0;url=https://evil\"><p>ok</p>",
                banned: &["http-equiv", "refresh", "evil"],
                kept: &["<p>ok</p>"],
            },
        ];
        for case in cases {
            let out = sanitize_html(case.input, 20_000);
            for banned in case.banned {
                assert!(
                    !out.to_lowercase().contains(&banned.to_lowercase()),
                    "case {} leaked {banned:?}: {out:?}",
                    case.name
                );
            }
            for kept in case.kept {
                assert!(
                    out.contains(kept),
                    "case {} dropped {kept:?}: {out:?}",
                    case.name
                );
            }
        }
    }

    #[test]
    fn sanitize_truncates_around_20kb_with_marker() {
        let big = "a".repeat(25_000);
        let out = sanitize_html(&big, 20_000);
        assert!(out.len() > 20_000 - 20 && out.len() <= 20_000 + "\n...[truncated]".len());
        assert!(out.ends_with("\n...[truncated]"));
        assert!(out.starts_with('a'));
        // multi-byte input truncates on a char boundary
        let multi = "é".repeat(15_000);
        let out = sanitize_html(&multi, 20_000);
        assert!(out.ends_with("\n...[truncated]"));
        assert_eq!(out.chars().last().unwrap(), ']');
    }

    #[tokio::test]
    async fn fetch_sanitized_extract_sanitizes_remote_html() {
        let html = "<html><body><h1>Title</h1>\
             <script>alert(1)</script>\
             <p onclick=\"evil()\">Body</p>\
             <a href=\"javascript:steal()\">x</a>\
             <iframe src=\"https://evil\"></iframe>\
             <meta http-equiv=\"refresh\" content=\"0\">\
             <base href=\"https://evil/\">\
             <style>p{}</style>\
             </body></html>";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    html.len(),
                    html
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        let extract = fetch_sanitized_extract(&format!("http://{addr}/")).await;
        server.await.unwrap();
        let lower = extract.to_lowercase();
        for banned in [
            "<script",
            "alert",
            "onclick",
            "evil()",
            "javascript:",
            "<iframe",
            "<meta",
            "<base",
            "<style",
        ] {
            assert!(!lower.contains(banned), "leaked {banned:?}: {extract:?}");
        }
        assert!(extract.contains("Title"));
        assert!(extract.contains("Body"));
    }
}
