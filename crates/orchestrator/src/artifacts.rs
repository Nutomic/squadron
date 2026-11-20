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
/// disk: `sprite exec` a `tar -C workdir -czf - . | base64` pull plus a remote
/// `tokei --output json`, then write both under the data dir.
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
    let bytes = decode_base64(&b64).map_err(|e| format!("sprite tar pull decode: {e}"))?;
    if bytes.is_empty() {
        return Err("sprite tar pull: empty archive".to_owned());
    }
    tokio::fs::write(&tarball_path, &bytes)
        .await
        .map_err(|e| format!("write tarball: {e}"))?;

    // Run tokei on the sprite itself.
    let tokei_json = match sprite
        .exec(sprite_name, &cd("tokei --output json ."), &opts)
        .await
    {
        Ok(out) if out.exit_code == 0 && !out.stdout.trim().is_empty() => {
            out.stdout.trim().to_owned()
        }
        _ => String::new(),
    };
    let (files_count, loc_count) = parse_tokei_totals(&tokei_json);

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

    if files_count.is_none() {
        return Err("sprite tokei produced no totals".to_owned());
    }
    Ok((
        tarball_path.to_string_lossy().to_string(),
        tokei_json,
        files_count,
        loc_count,
    ))
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
}
