//! F9: pre-embedded evidence for the validator turn.
//!
//! The curl-floor validator has NO shell: left to itself it only ever saw the
//! implementer's own claims and produced junk verdicts ("No file contents,
//! command output, or HTTP verification provided; implementer claims remain
//! unverified", quality_score 0/10). Before the validator turn the
//! orchestrator therefore gathers bounded evidence via `sprite exec` (the
//! workdir file list, the first ~8KB of up to ~5 key files, and an HTTP probe
//! of the implementer's public URL) and embeds it in the validator prompt's
//! DATA blocks. Everything gathered is untrusted content: it travels as DATA
//! and is never treated as instructions.

use std::time::Duration;

use base64::Engine as _;
use prompts::data_block;
use sprites_client::{ExecOptions, SpriteClient};

use crate::harness::{sanitize_excerpt, shell_quote};
use crate::net;

/// Per-file excerpt cap (~8KB of text per key file).
pub const EVIDENCE_FILE_CAP_BYTES: usize = 8 * 1024;
/// Maximum number of key files excerpted.
pub const EVIDENCE_MAX_FILES: usize = 5;
/// Maximum number of non-text-extension files content-checked so binaries
/// can be marked instead of silently dropped.
pub const EVIDENCE_MAX_BINARY_CANDIDATES: usize = 8;
/// File-list entries carried into the evidence block.
pub const EVIDENCE_LIST_MAX_ENTRIES: usize = 100;
/// HTTP-probe body prefix cap (~2KB of HTML).
pub const EVIDENCE_HTTP_CAP_BYTES: usize = 2 * 1024;
/// Bounded wall-clock for one evidence exec (the validator turn must not be
/// eaten by evidence gathering).
const EVIDENCE_EXEC_TIMEOUT: Duration = Duration::from_secs(60);

/// Text-ish extensions eligible for file excerpts (binaries are skipped: the
/// evidence is prompt-embedded text, not a file dump).
const TEXT_EXTENSIONS: [&str; 24] = [
    "html", "htm", "css", "js", "mjs", "cjs", "jsx", "ts", "tsx", "json", "md", "txt", "py", "rs",
    "go", "java", "sh", "vue", "svelte", "cfg", "toml", "yaml", "yml", "xml",
];

/// Gather the validator evidence blocks. Best-effort end to end: a failed
/// step yields an explicit "unavailable" block (visible evidence beats a
/// silent gap), and never fails the validator turn.
pub async fn gather_validator_evidence(
    sprite: &SpriteClient,
    sprite_name: &str,
    workdir: &str,
    impl_url: &str,
    allow_loopback: bool,
) -> String {
    let mut blocks = String::new();

    // 1. The workdir file list (bounded).
    let wd = shell_quote(workdir);
    let list_cmd = format!(
        "mkdir -p {wd} && cd {wd} && find . -type f -not -path '*/node_modules/*' -not -path './.git/*' | head -{EVIDENCE_LIST_MAX_ENTRIES}"
    );
    let file_list: Option<String> = match sprite
        .exec(
            sprite_name,
            &list_cmd,
            &ExecOptions::default().with_timeout(EVIDENCE_EXEC_TIMEOUT),
        )
        .await
    {
        Ok(out) if out.exit_code == 0 => Some(out.stdout),
        Ok(out) => {
            blocks.push_str(&data_block(
                "workdir-file-list",
                &format!("(read failed: exit {})", out.exit_code),
            ));
            None
        }
        Err(e) => {
            blocks.push_str(&data_block(
                "workdir-file-list",
                &format!(
                    "(unavailable: sprite exec failed: {})",
                    sanitize_excerpt(&e.to_string(), 300)
                ),
            ));
            None
        }
    };
    if let Some(list) = &file_list {
        blocks.push_str(&data_block(
            "workdir-file-list",
            &sanitize_text(list.trim(), 4096),
        ));
    }

    // 2. Excerpts of up to N key files (priority: index.html, package.json,
    //    then shallow text files; skip binaries).
    let mut excerpted: Vec<String> = Vec::new();
    if let Some(list) = &file_list {
        excerpted = select_key_files(list);
        for path in &excerpted {
            let read_cmd = format!(
                "cd {wd} && head -c {EVIDENCE_FILE_CAP_BYTES} {} | base64",
                shell_quote(path)
            );
            match sprite
                .exec(
                    sprite_name,
                    &read_cmd,
                    &ExecOptions::default().with_timeout(EVIDENCE_EXEC_TIMEOUT),
                )
                .await
            {
                Ok(out) if out.exit_code == 0 => match decode_b64_bytes(&out.stdout) {
                    Some(bytes) if is_binary_content(&bytes) => {
                        blocks.push_str(&data_block(
                            &format!("file-{path}"),
                            &binary_marker(bytes.len() as u64),
                        ));
                    }
                    Some(bytes) => {
                        let capped: String = String::from_utf8_lossy(&bytes)
                            .chars()
                            .take(EVIDENCE_FILE_CAP_BYTES)
                            .collect();
                        blocks.push_str(&data_block(&format!("file-{path}"), &capped));
                    }
                    None => {
                        blocks.push_str(&data_block(
                            &format!("file-{path}"),
                            "(unavailable: base64 decode failed)",
                        ));
                    }
                },
                Ok(out) => {
                    blocks.push_str(&data_block(
                        &format!("file-{path}"),
                        &format!("(read failed: exit {})", out.exit_code),
                    ));
                }
                Err(e) => {
                    blocks.push_str(&data_block(
                        &format!("file-{path}"),
                        &format!(
                            "(unavailable: sprite exec failed: {})",
                            sanitize_excerpt(&e.to_string(), 300)
                        ),
                    ));
                }
            }
        }
    }

    // 2b. Binary files: non-text-extension entries are content-checked and
    //     MARKED (`[binary file, N bytes, not embedded]`) instead of being
    //     silently dropped — the validator must be able to tell that an
    //     asset exists but was not embedded.
    if let Some(list) = &file_list {
        for path in select_binary_candidates(list, &excerpted) {
            let read_cmd = format!(
                "cd {wd} && wc -c < {} && head -c {EVIDENCE_FILE_CAP_BYTES} {} | base64",
                shell_quote(&path),
                shell_quote(&path)
            );
            match sprite
                .exec(
                    sprite_name,
                    &read_cmd,
                    &ExecOptions::default().with_timeout(EVIDENCE_EXEC_TIMEOUT),
                )
                .await
            {
                Ok(out) if out.exit_code == 0 => {
                    let (size, bytes) = split_size_and_body(&out.stdout);
                    match bytes {
                        Some(bytes) if is_binary_content(&bytes) => {
                            blocks.push_str(&data_block(
                                &format!("binary-{path}"),
                                &binary_marker(size.unwrap_or(bytes.len() as u64)),
                            ));
                        }
                        Some(bytes) => {
                            let capped: String = String::from_utf8_lossy(&bytes)
                                .chars()
                                .take(EVIDENCE_FILE_CAP_BYTES)
                                .collect();
                            blocks.push_str(&data_block(&format!("file-{path}"), &capped));
                        }
                        None => {
                            blocks.push_str(&data_block(
                                &format!("binary-{path}"),
                                "(unavailable: base64 decode failed)",
                            ));
                        }
                    }
                }
                Ok(out) => {
                    blocks.push_str(&data_block(
                        &format!("binary-{path}"),
                        &format!("(read failed: exit {})", out.exit_code),
                    ));
                }
                Err(e) => {
                    blocks.push_str(&data_block(
                        &format!("binary-{path}"),
                        &format!(
                            "(unavailable: sprite exec failed: {})",
                            sanitize_excerpt(&e.to_string(), 300)
                        ),
                    ));
                }
            }
        }
    }

    // 3. HTTP probe of the implementer's public URL: status + first ~2KB.
    match net::fetch_public_status_body(impl_url, allow_loopback, EVIDENCE_HTTP_CAP_BYTES).await {
        Ok((status, body)) => {
            let body = if body.trim().is_empty() {
                "(empty body)".to_owned()
            } else {
                body
            };
            blocks.push_str(&data_block(
                "http-probe",
                &format!("GET {impl_url} -> HTTP {status}\n\n{body}"),
            ));
        }
        Err(e) => {
            blocks.push_str(&data_block(
                "http-probe",
                &format!("GET {impl_url} failed: {}", sanitize_excerpt(&e, 300)),
            ));
        }
    }

    blocks
}

/// Control-char sanitizer that PRESERVES newlines (file lists stay readable
/// line-per-line) and caps the length on a char boundary.
fn sanitize_text(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| {
            if c == '\n' || c.is_ascii_graphic() || c == ' ' || c == '\t' {
                c
            } else {
                ' '
            }
        })
        .take(max)
        .collect()
}

/// Decode whitespace-wrapped base64 output (padding indifferent) to bytes.
fn decode_b64_bytes(s: &str) -> Option<Vec<u8>> {
    use base64::engine::DecodePaddingMode;
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    engine.decode(cleaned.as_bytes()).ok()
}

/// Decode base64 to text (lossy on invalid UTF-8).
#[cfg(test)]
fn decode_b64(s: &str) -> Option<String> {
    decode_b64_bytes(s).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Heuristic binary detection: NUL bytes or invalid UTF-8.
fn is_binary_content(bytes: &[u8]) -> bool {
    bytes.contains(&0) || String::from_utf8(bytes.to_vec()).is_err()
}

/// Marker emitted in place of binary file content (the validator must see
/// that the asset exists but was not embedded).
fn binary_marker(size: u64) -> String {
    format!("[binary file, {size} bytes, not embedded] (binary content skipped)")
}

/// Split a `wc -c < f && head -c ... f | base64` output into (size, body):
/// the first line is the byte size, the remainder is the base64 body.
fn split_size_and_body(stdout: &str) -> (Option<u64>, Option<Vec<u8>>) {
    match stdout.split_once('\n') {
        Some((size_line, b64)) => {
            let size = size_line.trim().parse::<u64>().ok();
            (size, decode_b64_bytes(b64))
        }
        None => (stdout.trim().parse::<u64>().ok(), None),
    }
}

/// Pick up to `EVIDENCE_MAX_FILES` key files from a `find` output (paths are
/// "./rel", one per line): index.html and package.json first, then the
/// shallowest text-extension files (site entry points before boilerplate).
fn select_key_files(list: &str) -> Vec<String> {
    let mut files: Vec<(usize, String)> = Vec::new();
    for line in list.lines() {
        let raw = line.trim().trim_start_matches("./");
        if raw.is_empty()
            || !raw.contains('.')
            || raw.contains("..")
            || raw.contains("node_modules")
        {
            continue;
        }
        let name = raw.rsplit('/').next().unwrap_or(raw);
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
        let priority = if name == "index.html" {
            0
        } else if name == "package.json" {
            1
        } else if ext
            .as_deref()
            .map(|e| TEXT_EXTENSIONS.contains(&e))
            .unwrap_or(false)
        {
            2
        } else {
            // binaries and anything else are skipped
            continue;
        };
        files.push((priority, raw.to_owned()));
    }
    // shallow paths first within a priority band (src/app.js before
    // src/components/deep/widget.js)
    files.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.matches('/').count().cmp(&b.1.matches('/').count()))
            .then(a.1.cmp(&b.1))
    });
    files.dedup_by(|a, b| a.1 == b.1);
    files
        .into_iter()
        .take(EVIDENCE_MAX_FILES)
        .map(|(_, p)| p)
        .collect()
}

/// Pick up to `EVIDENCE_MAX_BINARY_CANDIDATES` non-text-extension files from
/// a `find` output that were not already excerpted as key files: these are
/// content-checked so binaries can be marked, not silently dropped.
fn select_binary_candidates(list: &str, already: &[String]) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for line in list.lines() {
        let raw = line.trim().trim_start_matches("./");
        if raw.is_empty()
            || !raw.contains('.')
            || raw.contains("..")
            || raw.contains("node_modules")
        {
            continue;
        }
        if already.iter().any(|p| p == raw) {
            continue;
        }
        let name = raw.rsplit('/').next().unwrap_or(raw);
        let is_text = name
            .rsplit_once('.')
            .map(|(_, e)| TEXT_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
            .unwrap_or(false);
        if is_text {
            continue;
        }
        files.push(raw.to_owned());
    }
    files.sort_by(|a, b| {
        a.matches('/')
            .count()
            .cmp(&b.matches('/').count())
            .then(a.cmp(b))
    });
    files.dedup();
    files
        .into_iter()
        .take(EVIDENCE_MAX_BINARY_CANDIDATES)
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn select_key_files_prioritizes_entry_points_and_skips_binaries() {
        let list = "./node_modules/x.js\n./index.html\n./package.json\n./src/main.js\n./site.css\n./logo.png\n./data.bin\n./deep/a/b/c/d/app.js\n";
        let picked = select_key_files(list);
        assert_eq!(picked[0], "index.html");
        assert_eq!(picked[1], "package.json");
        assert!(picked.contains(&"src/main.js".to_owned()));
        assert!(picked.contains(&"site.css".to_owned()));
        assert!(
            picked.iter().all(|p| !p.contains("node_modules")
                && !p.ends_with(".png")
                && !p.ends_with(".bin")),
            "binaries must be skipped: {picked:?}"
        );
        assert!(picked.len() <= EVIDENCE_MAX_FILES, "{picked:?}");
        // the shallower text file outranks the nested one (capped at 5)
        assert_eq!(picked[2], "site.css", "shallow files first: {picked:?}");
        assert_eq!(picked[3], "src/main.js", "shallow files first: {picked:?}");
    }

    #[test]
    fn sanitize_text_keeps_newlines_and_strips_control_bytes() {
        assert_eq!(sanitize_text("a\nb\u{1}c", 100), "a\nb c");
        assert_eq!(sanitize_text("héllo", 100), "h llo");
        assert_eq!(sanitize_text("abcdef", 3), "abc");
    }

    #[test]
    fn decode_b64_tolerates_wrapping_and_missing_padding() {
        assert_eq!(decode_b64("aGVs\nbG8=\n").as_deref(), Some("hello"));
        assert_eq!(decode_b64("aGVsbG8").as_deref(), Some("hello"));
        assert_eq!(decode_b64("!!!"), None);
    }

    #[test]
    fn binary_markers_carry_size_and_skipped_note() {
        assert_eq!(
            binary_marker(9),
            "[binary file, 9 bytes, not embedded] (binary content skipped)"
        );
    }

    #[test]
    fn is_binary_content_detects_nul_and_non_utf8() {
        assert!(is_binary_content(b"\x00\x01\x02binary"));
        assert!(is_binary_content(&[0xFF, 0xFE]));
        assert!(!is_binary_content(b"plain text\n"));
        assert!(!is_binary_content(b""));
    }

    #[test]
    fn select_binary_candidates_picks_non_text_files_only() {
        let list = "./index.html\n./package.json\n./src/app.js\n./logo.png\n./assets/img/favicon.ico\n./node_modules/x.js\n";
        let picked = select_binary_candidates(
            list,
            &[
                "index.html".to_owned(),
                "package.json".to_owned(),
                "src/app.js".to_owned(),
            ],
        );
        assert_eq!(
            picked,
            vec!["logo.png".to_owned(), "assets/img/favicon.ico".to_owned()]
        );
    }

    #[test]
    fn split_size_and_body_parses_wc_line_and_b64() {
        let (size, body) = split_size_and_body("9\nAAEC\n");
        assert_eq!(size, Some(9));
        assert_eq!(body.as_deref(), Some(&[0x00, 0x01, 0x02][..]));
        let (size, body) = split_size_and_body("7");
        assert_eq!(size, Some(7));
        assert_eq!(body, None);
    }
}
