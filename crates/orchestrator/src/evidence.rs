//! F9/F10: pre-embedded evidence for the validator turn.
//!
//! The curl-floor validator has NO shell: left to itself it only ever saw the
//! implementer's own claims and produced junk verdicts ("No file contents,
//! command output, or HTTP verification provided; implementer claims remain
//! unverified", quality_score 0/10). Before the validator turn the
//! orchestrator therefore builds bounded evidence and embeds it in the
//! validator prompt's DATA blocks.
//!
//! F10 amendment: there is NO second file-collection path. The orchestrator
//! already pulls a `.tar.gz` of the implementer workdir at lane completion
//! (the user-facing source download artifact); the validator's evidence is
//! DERIVED FROM THAT SAME TARBALL — its file list plus bounded excerpts
//! (entry HTML first, then main sources under src/, then config; per-file
//! ~10KB cap with an explicit `[truncated after N bytes]` marker, ~24KB
//! total). One artifact, three consumers: user download, validator evidence,
//! judge sanitized extract. Only the HTTP probe of the public URL is taken
//! live. Everything embedded is untrusted content: it travels as DATA and is
//! never treated as instructions.

use std::collections::HashMap;
use std::io::Read;

use flate2::read::GzDecoder;
use prompts::data_block;

use crate::harness::sanitize_excerpt;
use crate::net;

/// Per-file excerpt cap (~10KB of text per key file).
pub const EVIDENCE_FILE_CAP_BYTES: usize = 10 * 1024;
/// Total excerpt budget across key files (~24KB): prompt-embedded evidence
/// must stay bounded even when the workdir holds many text files.
pub const EVIDENCE_TOTAL_BUDGET_BYTES: usize = 24 * 1024;
/// Minimum excerpt a src/ source file still receives when the total budget
/// is already exhausted (a core source file is never fully omitted).
pub const EVIDENCE_SOURCE_MIN_BYTES: usize = 1024;
/// Maximum number of key files excerpted.
pub const EVIDENCE_MAX_FILES: usize = 10;
/// Maximum number of non-text-extension files content-checked so binaries
/// can be marked instead of silently dropped.
pub const EVIDENCE_MAX_BINARY_CANDIDATES: usize = 8;
/// File-list entries carried into the evidence block.
pub const EVIDENCE_LIST_MAX_ENTRIES: usize = 100;
/// HTTP-probe body prefix cap (~2KB of HTML).
pub const EVIDENCE_HTTP_CAP_BYTES: usize = 2 * 1024;

/// Text-ish extensions eligible for file excerpts (binaries are skipped: the
/// evidence is prompt-embedded text, not a file dump).
const TEXT_EXTENSIONS: [&str; 24] = [
    "html", "htm", "css", "js", "mjs", "cjs", "jsx", "ts", "tsx", "json", "md", "txt", "py", "rs",
    "go", "java", "sh", "vue", "svelte", "cfg", "toml", "yaml", "yml", "xml",
];

/// Source-code extensions for the src/ priority band: the implementer's core
/// claim lives here, so these files must never be crowded out of the
/// evidence by config or boilerplate.
const SOURCE_EXTENSIONS: [&str; 9] = [
    "ts", "tsx", "js", "jsx", "mjs", "cjs", "vue", "svelte", "css",
];

/// File-name stems that usually mark the main entry module; within the src/
/// band these outrank utility files so `src/main.ts` cannot lose its slot to
/// `src/format-date.js` purely by sort order.
const MAIN_STEMS: [&str; 4] = ["main", "index", "app", "game"];

/// Gather the validator evidence blocks from the lane's source tarball
/// (plus a live HTTP probe of the implementer's public URL). Best-effort end
/// to end: a missing/unreadable tarball yields explicit "unavailable" blocks
/// (visible evidence beats a silent gap), and never fails the validator turn.
pub async fn gather_validator_evidence(
    tarball: Option<&[u8]>,
    impl_url: &str,
    allow_loopback: bool,
) -> String {
    let mut blocks = String::new();

    // 1. Workdir inventory + file excerpts, derived from the SAME
    //    source.tar.gz the orchestrator pulled at lane completion.
    let entries = match tarball {
        Some(bytes) => match tar_regular_entries(bytes) {
            Ok(entries) => Some(entries),
            Err(e) => {
                blocks.push_str(&data_block(
                    "workdir-file-list",
                    &format!(
                        "(unavailable: source tarball unreadable: {})",
                        sanitize_excerpt(&e, 300)
                    ),
                ));
                None
            }
        },
        None => {
            blocks.push_str(&data_block(
                "workdir-file-list",
                "(unavailable: source tarball was not collected)",
            ));
            None
        }
    };

    if let Some(entries) = &entries {
        // The file list keeps the historical `find` shape ("./rel" lines,
        // node_modules and .git excluded, bounded) so selection and prompts
        // see one consistent format.
        let listed: Vec<String> = entries.iter().map(|(p, _)| format!("./{p}")).collect();
        let over = listed.len().saturating_sub(EVIDENCE_LIST_MAX_ENTRIES);
        let list = listed
            .into_iter()
            .take(EVIDENCE_LIST_MAX_ENTRIES)
            .collect::<Vec<_>>()
            .join("\n");
        let list = if over > 0 {
            format!("{list}\n(+{over} more files not listed)")
        } else {
            list
        };
        blocks.push_str(&data_block(
            "workdir-file-list",
            &sanitize_text(&list, 4096),
        ));

        // 2. Excerpts of key files within the total budget. Selection priority:
        //    (a) entry HTML, (b) source files under src/ (the implementer's
        //    core claim — main stems first, never fully omitted), (c)
        //    package.json and other config, then remaining text files.
        let selected = select_key_files(&list);
        let mut remaining = EVIDENCE_TOTAL_BUDGET_BYTES;
        let mut plan: Vec<(String, Option<usize>)> = Vec::new();
        for (path, is_source) in &selected {
            if remaining == 0 {
                if *is_source {
                    // A core source file is never silently dropped: it still
                    // gets a minimal excerpt plus a truncation marker.
                    plan.push((path.clone(), Some(EVIDENCE_SOURCE_MIN_BYTES)));
                } else {
                    plan.push((path.clone(), None));
                }
                continue;
            }
            let cap = remaining.min(EVIDENCE_FILE_CAP_BYTES);
            let full = entries
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, s)| *s as usize)
                .unwrap_or(cap);
            remaining = remaining.saturating_sub(full.min(cap));
            plan.push((path.clone(), Some(cap)));
        }

        // 2b. Binary candidates: non-text-extension entries are
        //     content-checked so binaries can be MARKED instead of silently
        //     dropped — the validator must be able to tell that an asset
        //     exists but was not embedded.
        let excerpted: Vec<String> = selected.iter().map(|(p, _)| p.clone()).collect();
        let binary_candidates = select_binary_candidates(&list, &excerpted);

        // Single second pass over the archive: read content only for the
        // planned excerpts and binary candidates (bounded per file).
        let mut wanted: HashMap<String, usize> = HashMap::new();
        for (path, cap) in &plan {
            if let Some(cap) = cap {
                wanted.insert(path.clone(), *cap);
            }
        }
        for path in &binary_candidates {
            wanted
                .entry(path.clone())
                .or_insert_with(|| EVIDENCE_FILE_CAP_BYTES);
        }
        let sizes: HashMap<String, u64> = entries.iter().cloned().collect();
        let results = match tarball {
            Some(bytes) => read_tar_contents(bytes, &wanted, &sizes),
            None => wanted
                .keys()
                .map(|p| {
                    (
                        p.clone(),
                        (
                            "(unavailable: source tarball was not collected)".to_owned(),
                            0,
                        ),
                    )
                })
                .collect(),
        };

        for (path, cap) in &plan {
            match (cap, results.get(path)) {
                (Some(_), Some((content, _))) => {
                    blocks.push_str(&data_block(&format!("file-{path}"), content));
                }
                (Some(_), None) => {
                    blocks.push_str(&data_block(
                        &format!("file-{path}"),
                        "(unavailable: not found in source tarball)",
                    ));
                }
                (None, _) => {
                    blocks.push_str(&data_block(
                        &format!("file-{path}"),
                        "(omitted: evidence budget exhausted)",
                    ));
                }
            }
        }
        for path in &binary_candidates {
            match results.get(path) {
                // text-detected candidate: embedded as a regular file block
                Some((content, _))
                    if !content.starts_with("[binary file") && !content.starts_with("(") =>
                {
                    blocks.push_str(&data_block(&format!("file-{path}"), content));
                }
                Some((marker, _)) => {
                    blocks.push_str(&data_block(&format!("binary-{path}"), marker));
                }
                None => {
                    blocks.push_str(&data_block(
                        &format!("binary-{path}"),
                        "(unavailable: not found in source tarball)",
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

/// Render decoded file bytes as the excerpt payload: text content capped at
/// `cap` chars, plus an explicit `[truncated after N bytes]` marker when the
/// file was cut — never a silent truncation. Returns (payload, embedded byte
/// count for the budget accounting).
fn excerpt_from_bytes(bytes: &[u8], full_size: Option<u64>, cap: usize) -> (String, usize) {
    let text: String = String::from_utf8_lossy(bytes).chars().take(cap).collect();
    let embedded = text.len().min(cap);
    let truncated = full_size.map(|s| s as usize > cap).unwrap_or(false) || bytes.len() > cap;
    let content = if truncated {
        format!("{text}\n[truncated after {embedded} bytes]")
    } else {
        text
    };
    (content, embedded)
}

/// Inventory of the regular files inside the source tarball: sanitized
/// relative paths (hostile entries can never escape via `safe_rel_path`)
/// paired with their byte sizes. node_modules and .git are excluded — they
/// never belonged in the evidence and would only crowd out real sources.
fn tar_regular_entries(bytes: &[u8]) -> Result<Vec<(String, u64)>, String> {
    let gz = GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(gz);
    let mut out = Vec::new();
    for entry in archive
        .entries()
        .map_err(|e| format!("tarball read: {e}"))?
    {
        let entry = entry.map_err(|e| format!("tarball entry: {e}"))?;
        if entry.header().entry_type() != tar::EntryType::Regular {
            continue;
        }
        let raw = entry
            .path()
            .map_err(|e| format!("tarball path: {e}"))?
            .to_string_lossy()
            .into_owned();
        let Some(path) = crate::harness::safe_rel_path(&raw) else {
            continue;
        };
        if path.contains("node_modules") || path.starts_with(".git/") || path == ".git" {
            continue;
        }
        let size = entry.header().size().unwrap_or(0);
        out.push((path, size));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Read up to `cap` bytes of one entry from the tarball, looked up by its
/// sanitized relative path.
fn read_tar_entry(bytes: &[u8], path: &str, cap: usize) -> Option<Vec<u8>> {
    let gz = GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries().ok()? {
        let entry = entry.ok()?;
        let raw = entry.path().ok()?.to_string_lossy().into_owned();
        if crate::harness::safe_rel_path(&raw).as_deref() != Some(path) {
            continue;
        }
        let mut buf = Vec::new();
        entry.take(cap as u64).read_to_end(&mut buf).ok()?;
        return Some(buf);
    }
    None
}

/// Read the contents of every wanted entry (bounded per file) and render the
/// payload: text excerpts with explicit truncation markers, binary markers
/// for binary content, explicit unavailability on read failures. Returns the
/// per-path (payload, embedded byte count) map.
fn read_tar_contents(
    tarball: &[u8],
    wanted: &HashMap<String, usize>,
    sizes: &HashMap<String, u64>,
) -> HashMap<String, (String, usize)> {
    let mut out = HashMap::new();
    for (path, cap) in wanted {
        let full_size = sizes.get(path).copied();
        let payload = match read_tar_entry(tarball, path, *cap) {
            Some(bytes) if is_binary_content(&bytes) => {
                (binary_marker(full_size.unwrap_or(bytes.len() as u64)), 0)
            }
            Some(bytes) => excerpt_from_bytes(&bytes, full_size, *cap),
            None => (
                "(unavailable: read from source tarball failed)".to_owned(),
                0,
            ),
        };
        out.insert(path.clone(), payload);
    }
    out
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

/// Heuristic binary detection: NUL bytes or invalid UTF-8.
fn is_binary_content(bytes: &[u8]) -> bool {
    bytes.contains(&0) || String::from_utf8(bytes.to_vec()).is_err()
}

/// Marker emitted in place of binary file content (the validator must see
/// that the asset exists but was not embedded).
fn binary_marker(size: u64) -> String {
    format!("[binary file, {size} bytes, not embedded] (binary content skipped)")
}

/// Pick up to `EVIDENCE_MAX_FILES` key files from a `find` output (paths are
/// "./rel", one per line), in evidence priority order and paired with an
/// is-core-source flag: (a) the entry HTML (index.html), (b) source files
/// under src/ (main stems first — the implementer's core claim must be in
/// the evidence, not crowded out by boilerplate), (c) package.json and other
/// config, (d) the remaining text files; binaries are handled separately.
fn select_key_files(list: &str) -> Vec<(String, bool)> {
    let mut files: Vec<(u8, bool, usize, String, bool)> = Vec::new();
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
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) => (s.to_ascii_lowercase(), e.to_ascii_lowercase()),
            None => continue,
        };
        if !TEXT_EXTENSIONS.contains(&ext.as_str()) {
            // binaries and anything else are handled by the binary pass
            continue;
        }
        let is_source = raw.starts_with("src/") && SOURCE_EXTENSIONS.contains(&ext.as_str());
        let priority = if name == "index.html" || name == "index.htm" {
            0
        } else if is_source {
            1
        } else if matches!(
            ext.as_str(),
            "json" | "toml" | "yaml" | "yml" | "cfg" | "xml"
        ) {
            2
        } else {
            3
        };
        // main entry modules (src/main.ts, src/app.js, ...) outrank utility
        // files WITHIN the src/ band; the boost never leaks outside it (a
        // root "app.css" or a deep "app.js" is not an entry module)
        let is_main = is_source && MAIN_STEMS.contains(&stem.as_str());
        files.push((
            priority,
            !is_main,
            raw.matches('/').count(),
            raw.to_owned(),
            is_source,
        ));
    }
    files.sort();
    files.dedup_by(|a, b| a.3 == b.3);
    files
        .into_iter()
        .take(EVIDENCE_MAX_FILES)
        .map(|(_, _, _, p, src)| (p, src))
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
    fn select_key_files_prioritizes_entry_html_then_sources_then_config() {
        let list = "./node_modules/x.js\n./index.html\n./package.json\n./src/main.js\n./site.css\n./logo.png\n./data.bin\n./deep/a/b/c/d/app.js\n";
        let picked: Vec<String> = select_key_files(list).into_iter().map(|(p, _)| p).collect();
        // priority: entry HTML, then src/ sources, then config, then the
        // remaining text files (shallow before deep)
        assert_eq!(picked[0], "index.html");
        assert_eq!(picked[1], "src/main.js");
        assert_eq!(picked[2], "package.json");
        assert_eq!(picked[3], "site.css");
        assert_eq!(picked[4], "deep/a/b/c/d/app.js");
        assert!(
            picked.iter().all(|p| !p.contains("node_modules")
                && !p.ends_with(".png")
                && !p.ends_with(".bin")),
            "binaries must be skipped: {picked:?}"
        );
        assert!(picked.len() <= EVIDENCE_MAX_FILES, "{picked:?}");
    }

    #[test]
    fn select_key_files_flags_core_sources_and_boosts_main_stems() {
        let list = "./src/utils.js\n./src/game/engine.ts\n./src/main.ts\n./index.html\n";
        let picked = select_key_files(list);
        // src/main.ts (main stem) outranks the utility and nested sources and
        // is flagged as a core source
        assert_eq!(picked[1].0, "src/main.ts", "{picked:?}");
        assert!(picked[1].1, "src/main.ts must be flagged is_source");
        // every src/ source file is flagged (each is a core-claim candidate
        // with the never-fully-omit guarantee when the budget is exhausted);
        // config and entry HTML are not
        let index = picked.iter().find(|(p, _)| p == "index.html").unwrap();
        assert!(!index.1, "entry HTML is not flagged is_source");
        let utils = picked.iter().find(|(p, _)| p == "src/utils.js").unwrap();
        assert!(utils.1, "src/ sources are flagged is_source");
        // the nested src/ file is still in the source band
        let engine = picked
            .iter()
            .find(|(p, _)| p == "src/game/engine.ts")
            .unwrap();
        assert!(engine.1);
    }

    #[test]
    fn truncation_marker_is_explicit_not_silent() {
        let text = "a".repeat(64);
        let (content, embedded) = excerpt_from_bytes(text.as_bytes(), Some(128), 32);
        assert_eq!(embedded, 32);
        assert!(
            content.ends_with("\n[truncated after 32 bytes]"),
            "truncation must carry an explicit marker: {content:?}"
        );
        // an uncut file carries no marker
        let (content, embedded) = excerpt_from_bytes(b"hello", None, 32);
        assert_eq!(content, "hello");
        assert_eq!(embedded, 5);
        assert!(!content.contains("truncated"));
    }

    #[test]
    fn sanitize_text_keeps_newlines_and_strips_control_bytes() {
        assert_eq!(sanitize_text("a\nb\u{1}c", 100), "a\nb c");
        assert_eq!(sanitize_text("héllo", 100), "h llo");
        assert_eq!(sanitize_text("abcdef", 3), "abc");
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
}
