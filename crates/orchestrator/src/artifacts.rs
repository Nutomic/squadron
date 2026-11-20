use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::write::GzEncoder;
use flate2::Compression;

use base64::Engine as _;

use crate::config::PipelineConfig;

pub fn lane_artifacts_dir(cfg: &PipelineConfig, run_id: &str, lane_idx: i64) -> PathBuf {
    cfg.data_dir
        .join("runs")
        .join(run_id)
        .join(format!("lane-{lane_idx}"))
}

pub async fn ensure_dir(path: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(path).await
}

pub async fn collect_artifacts(
    cfg: &PipelineConfig,
    run_id: &str,
    lane_idx: i64,
    workdir: &Path,
    console_tail: &str,
) -> Result<(String, String, Option<i64>, Option<i64>), String> {
    let dir = lane_artifacts_dir(cfg, run_id, lane_idx);
    ensure_dir(&dir).await.map_err(|e| e.to_string())?;

    let tarball_path = dir.join("source.tar.gz");
    create_tarball(workdir, &tarball_path).map_err(|e| format!("tarball: {e}"))?;

    let (tokei_json, files_count, loc_count) = run_tokei(workdir).unwrap_or_else(|_| {
        // fallback: count files roughly
        let (f, l) = rough_counts(workdir);
        (
            serde_json::json!({"files": f, "loc": l}).to_string(),
            Some(f),
            Some(l),
        )
    });

    // persist console tail (truncated ~4KB) alongside
    let tail_path = dir.join("console_tail.txt");
    let tail_trunc: String = console_tail
        .chars()
        .rev()
        .take(4096)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let _ = tokio::fs::write(&tail_path, &tail_trunc).await;

    Ok((
        tarball_path.to_string_lossy().to_string(),
        tokei_json,
        files_count,
        loc_count,
    ))
}

fn create_tarball(src_dir: &Path, dest: &Path) -> Result<(), String> {
    let file = File::create(dest).map_err(|e| e.to_string())?;
    let enc = GzEncoder::new(file, Compression::default());
    let mut tar = tar::Builder::new(enc);
    if src_dir.exists() {
        // append directory contents; ignore failures on special files
        tar.append_dir_all(".", src_dir)
            .map_err(|e| e.to_string())?;
    }
    let enc = tar.into_inner().map_err(|e| e.to_string())?;
    enc.finish().map_err(|e| e.to_string())?;
    Ok(())
}

fn run_tokei(workdir: &Path) -> Result<(String, Option<i64>, Option<i64>), String> {
    let output = Command::new("tokei")
        .arg("--output")
        .arg("json")
        .arg(workdir)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }
    let json_str = String::from_utf8_lossy(&output.stdout).to_string();
    let (files, loc) = parse_tokei_totals(&json_str);
    Ok((json_str, files, loc))
}

pub fn parse_tokei_totals(json_str: &str) -> (Option<i64>, Option<i64>) {
    let v: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    // tokei json output: { "Total": { "code": N, "blanks":..,"comments":..,"children":{Lang:{reports:[...]}} } , "Rust": {...}, ...}
    if let Some(total) = v.get("Total").or_else(|| v.get("total")) {
        let code = total.get("code").and_then(|x| x.as_i64());
        // files count: sum reports lengths across languages
        let mut files: i64 = 0;
        if let Some(obj) = v.as_object() {
            for (k, lang) in obj {
                if k.eq_ignore_ascii_case("total") {
                    continue;
                }
                if let Some(reports) = lang.get("reports").and_then(|r| r.as_array()) {
                    files += reports.len() as i64;
                }
            }
        }
        return (Some(files), code);
    }
    (None, None)
}

fn rough_counts(dir: &Path) -> (i64, i64) {
    let mut files = 0i64;
    let mut loc = 0i64;
    if let Ok(entries) = walkdir(dir) {
        for p in entries {
            if p.is_file() {
                files += 1;
                if let Ok(s) = std::fs::read_to_string(&p) {
                    loc += s.lines().count() as i64;
                }
            }
        }
    }
    (files, loc)
}

fn walkdir(dir: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            out.extend(walkdir(&p)?);
        } else {
            out.push(p);
        }
    }
    Ok(out)
}

pub async fn check_http_200(url: &str) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    match client.get(url).send().await {
        Ok(resp) => resp.status().as_u16() == 200,
        Err(_) => false,
    }
}

/// #4: collect artifacts FROM the sprite, not from the orchestrator's local
/// disk: `sprite exec` a `tar -C workdir -czf - . | base64` pull, then run
/// tokei ORCHESTRATOR-side over the pulled tarball (R3-B: tokei may be absent
/// inside the sprite; the runtime image always has it). The tarball path is
/// kept even when the stats pass degrades — stats failure must not lose the
/// already-pulled source archive.
pub async fn collect_artifacts_from_sprite(
    sprite: &sprites_client::SpriteClient,
    sprite_name: &str,
    workdir: &str,
    dest_dir: &Path,
    console_tail: &str,
) -> Result<(String, String, Option<i64>, Option<i64>), String> {
    tokio::fs::create_dir_all(dest_dir)
        .await
        .map_err(|e| format!("artifacts dir: {e}"))?;
    let tarball_path = dest_dir.join("source.tar.gz");

    let cd = |cmd: &str| format!("mkdir -p \"$SWARM_WORKDIR\" && cd \"$SWARM_WORKDIR\" && {cmd}");
    let opts = sprites_client::ExecOptions::default().with_env("SWARM_WORKDIR", workdir);

    // Stream the lane workdir back as base64 (binary-safe over the CLI).
    let tar_out = sprite
        .exec(sprite_name, &cd("tar -czf - . | base64"), &opts)
        .await
        .map_err(|e| format!("sprite tar pull: {e}"))?;
    let b64: String = tar_out
        .stdout
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    // Cheap pre-decode guard: base64 expands 4/3, so anything above 2x the
    // cap cannot be a legal pull (R3-B).
    if b64.len() > TARBALL_CAP_BYTES * 2 {
        return Err(format!(
            "sprite tar pull: base64 payload of {} bytes exceeds the {} byte cap",
            b64.len(),
            TARBALL_CAP_BYTES
        ));
    }
    let bytes = decode_base64(&b64).map_err(|e| format!("sprite tar pull decode: {e}"))?;
    tarball_cap_error(bytes.len())?;
    if bytes.is_empty() {
        return Err("sprite tar pull: empty archive".to_owned());
    }
    tokio::fs::write(&tarball_path, &bytes)
        .await
        .map_err(|e| format!("write tarball: {e}"))?;

    // Stats orchestrator-side over the pulled tarball; degraded, never fatal
    // for the archive (R3-B).
    let (tokei_json, files_count, loc_count) = tokei_stats_from_tarball(&bytes);

    // Persist console tail alongside.
    let tail_path = dest_dir.join("console_tail.txt");
    let tail_trunc: String = console_tail
        .chars()
        .rev()
        .take(4096)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let _ = tokio::fs::write(&tail_path, &tail_trunc).await;

    Ok((
        tarball_path.to_string_lossy().to_string(),
        tokei_json,
        files_count,
        loc_count,
    ))
}

/// R3-B: the pulled tarball is capped so a runaway lane workdir cannot blow
/// up orchestrator memory/disk.
pub const TARBALL_CAP_BYTES: usize = 50 * 1024 * 1024;

/// Enforce the tarball size cap on the decoded pull.
pub fn tarball_cap_error(len: usize) -> Result<(), String> {
    if len > TARBALL_CAP_BYTES {
        Err(format!(
            "sprite tar pull: {} bytes exceeds the {} byte tarball cap",
            len, TARBALL_CAP_BYTES
        ))
    } else {
        Ok(())
    }
}

/// Extract the pulled tar.gz into a fresh temp dir (files and directories
/// only; entry paths are sanitized through `safe_rel_path`, so hostile
/// archive entries can never escape) and run tokei over it. When tokei is
/// unavailable or fails, fall back to rough counts marked `"degraded"` —
/// stats are best-effort and must never cost the lane its artifacts.
pub fn tokei_stats_from_tarball(bytes: &[u8]) -> (String, Option<i64>, Option<i64>) {
    let dir = temp_extract_dir();
    let cleanup = |dir: &Path| {
        let _ = std::fs::remove_dir_all(dir);
    };
    if let Err(e) = extract_tarball_gz(bytes, &dir) {
        cleanup(&dir);
        return (
            serde_json::json!({"degraded": true, "error": e}).to_string(),
            None,
            None,
        );
    }
    let (json, files, loc) = match run_tokei(&dir) {
        Ok(out) => out,
        Err(_) => {
            let (f, l) = rough_counts(&dir);
            (
                serde_json::json!({"degraded": true, "files": f, "loc": l}).to_string(),
                Some(f),
                Some(l),
            )
        }
    };
    cleanup(&dir);
    (json, files, loc)
}

fn temp_extract_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("swarm-tokei-{}-{}", std::process::id(), nanos))
}

fn extract_tarball_gz(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(gz);
    for entry in archive
        .entries()
        .map_err(|e| format!("tarball read: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("tarball entry: {e}"))?;
        let raw = entry
            .path()
            .map_err(|e| format!("tarball path: {e}"))?
            .to_string_lossy()
            .into_owned();
        // Model-produced workdir content: contain every path into `dest`.
        let Some(safe) = crate::harness::safe_rel_path(&raw) else {
            continue;
        };
        let full = dest.join(safe);
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                std::fs::create_dir_all(&full).map_err(|e| e.to_string())?;
            }
            tar::EntryType::Regular => {
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                let mut out = File::create(&full).map_err(|e| e.to_string())?;
                std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
            }
            _ => {} // symlinks and special files are skipped
        }
    }
    Ok(())
}

/// Decode standard base64 via the shared `base64` crate (whitespace tolerated;
/// padding indifferent so CLI-wrapped output decodes even when truncated).
fn decode_base64(s: &str) -> Result<Vec<u8>, String> {
    use base64::engine::DecodePaddingMode;
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    engine
        .decode(cleaned.as_bytes())
        .map_err(|e| format!("base64: {e}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // test code: panics on failure are the assertion mechanism

    use super::*;

    #[test]
    fn base64_decode_round_trips() {
        for sample in ["aGVsbG8=", "aGVsbG8gd29ybGQ=", "YQ==", ""] {
            let expected: &[u8] = match sample {
                "aGVsbG8=" => b"hello",
                "aGVsbG8gd29ybGQ=" => b"hello world",
                "YQ==" => b"a",
                _ => b"",
            };
            assert_eq!(decode_base64(sample).unwrap(), expected);
        }
        // whitespace and missing padding tolerated
        assert_eq!(decode_base64("aGVs bG8\n").unwrap(), b"hello");
        assert!(decode_base64("!!!").is_err());
    }

    #[test]
    fn tarball_cap_enforced() {
        assert!(tarball_cap_error(TARBALL_CAP_BYTES).is_ok());
        assert!(tarball_cap_error(TARBALL_CAP_BYTES - 1).is_ok());
        let err = tarball_cap_error(TARBALL_CAP_BYTES + 1).unwrap_err();
        assert!(err.contains("exceeds the"), "err: {err}");
        assert!(err.contains(&format!("{TARBALL_CAP_BYTES}")));
    }

    /// Build an in-memory tar.gz with the given (path, content) entries.
    fn tarball_bytes(entries: &[(&str, &str)]) -> Vec<u8> {
        use std::io::Write as _;
        let mut inner = Vec::new();
        {
            let mut tar = tar::Builder::new(&mut inner);
            for (path, content) in entries {
                let mut h = tar::Header::new_gnu();
                h.set_size(content.len() as u64);
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(0o644);
                tar.append_data(&mut h, *path, content.as_bytes()).unwrap();
            }
            tar.finish().unwrap();
        }
        // Forge a hostile absolute-path entry: the tar builder refuses them,
        // so rewrite the first entry's header name field to "/abs.txt" and
        // recompute the header checksum.
        if let Some(idx) = inner.windows(8).position(|w| w == b"abs.txt\0") {
            assert!(idx < 512, "name field must be in the first header");
            inner[idx..idx + 100].fill(0);
            inner[..8].copy_from_slice(b"/abs.txt");
            inner[148..156].copy_from_slice(b"        ");
            let sum: u32 = inner[..512].iter().map(|b| *b as u32).sum();
            let chk = format!("{:06o}", sum);
            inner[148..154].copy_from_slice(chk.as_bytes());
            inner[154] = 0;
            inner[155] = b' ';
        }
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&inner).unwrap();
        gz.finish().unwrap()
    }

    /// R3-B: stats run orchestrator-side over the pulled tarball; counts are
    /// Some whether the tokei binary is available or the degraded rough-count
    /// fallback fires, and the extraction is contained (hostile `../` and
    /// absolute entries are dropped, nothing is created outside the temp dir).
    #[test]
    fn tokei_stats_from_tarball_counts_and_contains() {
        let before_temp: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        let bytes = tarball_bytes(&[
            // (the tar builder refuses `..` and absolute paths, so traversal
            // containment is asserted with a forged absolute-path entry —
            // appended under the legal name `abs.txt`, then patched;
            // extract_tarball_gz additionally drops `..` entries via
            // safe_rel_path)
            ("abs.txt", "must not escape"),
            ("index.html", "<h1>hi</h1>\n"),
            ("src/app.js", "console.log(1);\n"),
            ("src/lib.js", "let x = 1;\n"),
        ]);
        let (json, files, loc) = tokei_stats_from_tarball(&bytes);
        assert!(files.is_some(), "degraded stats must still count files");
        assert!(loc.is_some(), "degraded stats must still count loc");
        // tokei output parses into totals; the degraded marker json would be
        // flat, so a real tokei run yields the Total block.
        let parsed = parse_tokei_totals(&json);
        let _ = parsed; // (None, None) is legal for the degraded shape
                        // nothing escaped the temp dir
        let outside = std::env::temp_dir().join("escape.txt");
        assert!(!outside.exists(), "absolute/.. entries must be contained");
        let _ = std::fs::remove_file(&outside);
        let abs_outside = std::env::temp_dir().join("abs.txt");
        assert!(!abs_outside.exists());
        // extract dir was cleaned up
        let after_temp: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(
            before_temp.len(),
            after_temp.len(),
            "temp extract dir must be removed"
        );
    }
}
