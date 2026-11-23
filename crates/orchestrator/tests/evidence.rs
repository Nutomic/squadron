//! F9/F10: the validator's evidence is derived from the SAME source.tar.gz
//! the orchestrator pulls at lane completion (the user-facing source
//! download artifact) — its file list plus bounded excerpts (entry HTML,
//! main sources under src/, config) and a live HTTP probe of the public
//! URL, all embedded as DATA for the validator turn. There is no second
//! file-collection path.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use axum::extract::Request;
use axum::routing::get;
use axum::Router;
use orchestrator::evidence::gather_validator_evidence;
use tokio::sync::Mutex;

/// Build an in-memory tar.gz of a directory tree, exactly like the lane
/// artifact pull produces (`tar -czf - .` over the workdir).
fn tarball_from_dir(dir: &std::path::Path) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    {
        let mut tar = tar::Builder::new(&mut gz);
        tar.append_dir_all(".", dir).unwrap();
        tar.finish().unwrap();
    }
    gz.finish().unwrap()
}

/// Loopback tunnel answering 200 for the HTTP-probe part of the evidence.
async fn spawn_tunnel(hits: Arc<Mutex<u32>>) -> String {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().fallback(get(move |_req: Request| {
        let hits = hits.clone();
        async move {
            *hits.lock().await += 1;
            (axum::http::StatusCode::OK, "<html>probe body</html>")
        }
    }));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/site")
}

#[tokio::test(flavor = "multi_thread")]
async fn gather_validator_evidence_embeds_files_and_http_probe() {
    let dir = tempfile::tempdir().unwrap();
    // Populate the lane workdir the way an implementer would.
    let workdir = dir.path().join("squadron-work").join("lane-0");
    std::fs::create_dir_all(workdir.join("src")).unwrap();
    std::fs::create_dir_all(workdir.join("node_modules")).unwrap();
    std::fs::write(
        workdir.join("index.html"),
        "<html><body>EVIDENCE-MARKER-site-body</body></html>\n",
    )
    .unwrap();
    std::fs::write(workdir.join("package.json"), "{\"name\":\"site\"}\n").unwrap();
    std::fs::write(workdir.join("src/app.js"), "console.log(42);\n").unwrap();
    std::fs::write(workdir.join("logo.png"), b"\x00\x01\x02binary").unwrap();
    std::fs::write(workdir.join("node_modules/leftpad.js"), "junk\n").unwrap();
    let tarball = tarball_from_dir(&workdir);

    let hits = Arc::new(Mutex::new(0));
    let url = spawn_tunnel(hits.clone()).await;

    let evidence = gather_validator_evidence(Some(&tarball), &url, true).await;

    // File list block: real entries, node_modules excluded.
    assert!(
        evidence.contains("<data label=\"workdir-file-list\">"),
        "evidence: {evidence}"
    );
    assert!(evidence.contains("./index.html"), "evidence: {evidence}");
    assert!(evidence.contains("./src/app.js"), "evidence: {evidence}");
    assert!(
        !evidence.contains("node_modules"),
        "node_modules must never reach the evidence: {evidence}"
    );

    // Key-file excerpts carry the real content, bounded and text-only.
    assert!(
        evidence.contains("EVIDENCE-MARKER-site-body"),
        "index.html content must be excerpted: {evidence}"
    );
    assert!(
        evidence.contains("console.log(42);"),
        "evidence: {evidence}"
    );
    assert!(
        evidence.contains("<data label=\"file-index.html\">"),
        "evidence: {evidence}"
    );
    assert!(
        evidence.contains("<data label=\"file-package.json\">"),
        "evidence: {evidence}"
    );
    assert!(
        !evidence.contains("file-logo.png"),
        "binaries must be skipped: {evidence}"
    );
    assert!(
        evidence.contains("(binary content skipped)"),
        "binary files must be marked, not silently dropped: {evidence}"
    );

    // HTTP probe block: status + body of the live URL.
    assert!(
        evidence.contains("<data label=\"http-probe\">"),
        "evidence: {evidence}"
    );
    assert!(
        evidence.contains(&format!("GET {url} -> HTTP 200")),
        "probe status must be embedded: {evidence}"
    );
    assert!(
        evidence.contains("probe body"),
        "probe body prefix must be embedded: {evidence}"
    );
    // the probe actually hit the tunnel
    assert!(*hits.lock().await >= 1, "the HTTP probe must reach the URL");
}

/// F10 regression (run 8): the gatherer embedded index.html and
/// package.json but NEVER src/main.ts — the implementer's core gameplay
/// source — so every gameplay claim came back UNVERIFIED. The src/ source
/// band (main stems first) must guarantee the core source file is embedded.
#[tokio::test(flavor = "multi_thread")]
async fn gather_validator_evidence_embeds_the_core_src_source_files() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path().join("squadron-work").join("lane-2");
    std::fs::create_dir_all(workdir.join("src")).unwrap();
    std::fs::write(
        workdir.join("index.html"),
        "<html><body>game</body></html>\n",
    )
    .unwrap();
    std::fs::write(workdir.join("package.json"), "{\"name\":\"game\"}\n").unwrap();
    std::fs::write(
        workdir.join("src/main.ts"),
        "MAIN-TS-MARKER: const player = { x: 0, y: 0 };\n",
    )
    .unwrap();
    std::fs::write(
        workdir.join("src/utils.ts"),
        "UTILS-TS-MARKER: export const id = 1;\n",
    )
    .unwrap();
    std::fs::write(workdir.join("vite.config.ts"), "export default {};\n").unwrap();
    let tarball = tarball_from_dir(&workdir);

    let evidence =
        gather_validator_evidence(Some(&tarball), "https://203.0.113.1/nope", false).await;

    assert!(
        evidence.contains("<data label=\"file-src/main.ts\">"),
        "the core gameplay source MUST be embedded: {evidence}"
    );
    assert!(
        evidence.contains("MAIN-TS-MARKER"),
        "src/main.ts content must reach the validator: {evidence}"
    );
    assert!(
        evidence.contains("UTILS-TS-MARKER"),
        "src/ sources outrank config in the selection: {evidence}"
    );
    assert!(
        evidence.contains("<data label=\"file-vite.config.ts\">"),
        "config files still fit within the budget: {evidence}"
    );
    assert!(
        evidence.contains("<data label=\"file-package.json\">"),
        "evidence: {evidence}"
    );
}

/// A source file larger than the per-file cap is embedded cut, with an
/// explicit `[truncated after N bytes]` marker — never silently dropped.
#[tokio::test(flavor = "multi_thread")]
async fn gather_validator_evidence_truncates_large_sources_with_marker() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path().join("squadron-work").join("lane-3");
    std::fs::create_dir_all(workdir.join("src")).unwrap();
    std::fs::write(
        workdir.join("index.html"),
        "<html><body>game</body></html>\n",
    )
    .unwrap();
    let big = "BIGMAIN-MARKER ".repeat(40 * 1024 / 15);
    std::fs::write(workdir.join("src/main.ts"), big).unwrap();
    let tarball = tarball_from_dir(&workdir);

    let evidence =
        gather_validator_evidence(Some(&tarball), "https://203.0.113.1/nope", false).await;

    assert!(
        evidence.contains("<data label=\"file-src/main.ts\">"),
        "evidence: {evidence}"
    );
    assert!(
        evidence.contains("BIGMAIN-MARKER"),
        "the truncated source still carries its opening content: {evidence}"
    );
    assert!(
        evidence.contains("[truncated after 10240 bytes]"),
        "an explicit truncation marker is required: {evidence}"
    );
}

/// Best-effort: an unreachable public URL yields an explicit probe-failure
/// block instead of failing the validator turn.
#[tokio::test(flavor = "multi_thread")]
async fn gather_validator_evidence_reports_unreachable_probe_as_data() {
    let dir = tempfile::tempdir().unwrap();
    // Empty workdir: the tarball still carries the directory structure.
    let workdir = dir.path().join("squadron-work").join("lane-1");
    std::fs::create_dir_all(&workdir).unwrap();
    let tarball = tarball_from_dir(&workdir);

    // Unroutable documentation-range host: resolution/connect fails fast.
    let evidence =
        gather_validator_evidence(Some(&tarball), "https://203.0.113.1/nope", false).await;

    assert!(
        evidence.contains("<data label=\"http-probe\">"),
        "evidence: {evidence}"
    );
    assert!(
        evidence.contains("GET https://203.0.113.1/nope failed"),
        "the probe failure must be stated in DATA: {evidence}"
    );
}

/// F10 amendment: when the artifact pull failed (no tarball), the evidence
/// degrades explicitly — the validator sees WHY the file data is missing
/// and still gets the live HTTP probe. No silent gap, no fabricated files.
#[tokio::test(flavor = "multi_thread")]
async fn gather_validator_evidence_states_tarball_unavailability_explicitly() {
    let hits = Arc::new(Mutex::new(0));
    let url = spawn_tunnel(hits.clone()).await;

    let evidence = gather_validator_evidence(None, &url, true).await;

    assert!(
        evidence.contains("source tarball was not collected"),
        "the missing tarball must be stated in DATA: {evidence}"
    );
    assert!(
        evidence.contains("<data label=\"http-probe\">"),
        "the live probe is still embedded: {evidence}"
    );
    assert!(
        evidence.contains(&format!("GET {url} -> HTTP 200")),
        "evidence: {evidence}"
    );
    // no fabricated file blocks without the artifact
    assert!(
        !evidence.contains("<data label=\"file-"),
        "no file excerpts can exist without the tarball: {evidence}"
    );
}
