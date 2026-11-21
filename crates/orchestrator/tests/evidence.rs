//! F9: the orchestrator gathers bounded validator evidence via `sprite exec`
//! (workdir file list, first ~8KB of up to ~5 key files, HTTP probe of the
//! public URL) and embeds it as DATA for the validator turn. Driven against
//! the mock sprite CLI, which really executes the gathered commands.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use axum::extract::Request;
use axum::routing::get;
use axum::Router;
use orchestrator::evidence::gather_validator_evidence;
use sprites_client::{SpriteClient, SpriteConfig};
use tokio::sync::Mutex;

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test(flavor = "multi_thread")]
async fn gather_validator_evidence_embeds_files_and_http_probe() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let sprite = SpriteClient::new(SpriteConfig {
        cli_path: option_env!("CARGO_BIN_EXE_mock-sprite-cli")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/debug/mock-sprite-cli")
            }),
        command_timeout: Duration::from_secs(15),
        ..SpriteConfig::default()
    });
    sprite.create("ev-sprite").await.unwrap();
    sprite.set_url_auth_public("ev-sprite").await.unwrap();

    // Populate the lane workdir the way an implementer would.
    let workdir = dir.path().join("swarm-work").join("lane-0");
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

    // The implementer's public URL (loopback tunnel; test-only escape).
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let hits: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let hits_for_handler = hits.clone();
    let app = Router::new().fallback(get(move |_req: Request| {
        let hits = hits_for_handler.clone();
        async move {
            *hits.lock().await += 1;
            (axum::http::StatusCode::OK, "<html>probe body</html>")
        }
    }));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let url = format!("http://{addr}/site");

    let evidence =
        gather_validator_evidence(&sprite, "ev-sprite", workdir.to_str().unwrap(), &url, true)
            .await;

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

    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
    std::env::remove_var("MOCK_CREATE_MS");
    std::env::remove_var("MOCK_EXEC_MS");
}

/// F10 regression (run 8): the gatherer embedded index.html and
/// package.json but NEVER src/main.ts — the implementer's core gameplay
/// source — so every gameplay claim came back UNVERIFIED. The src/ source
/// band (main stems first) must guarantee the core source file is embedded.
#[tokio::test(flavor = "multi_thread")]
async fn gather_validator_evidence_embeds_the_core_src_source_files() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state3.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let sprite = SpriteClient::new(SpriteConfig {
        cli_path: option_env!("CARGO_BIN_EXE_mock-sprite-cli")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/debug/mock-sprite-cli")
            }),
        command_timeout: Duration::from_secs(15),
        ..SpriteConfig::default()
    });
    sprite.create("ev-sprite3").await.unwrap();
    sprite.set_url_auth_public("ev-sprite3").await.unwrap();

    let workdir = dir.path().join("swarm-work").join("lane-2");
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

    let evidence = gather_validator_evidence(
        &sprite,
        "ev-sprite3",
        workdir.to_str().unwrap(),
        "https://203.0.113.1/nope",
        false,
    )
    .await;

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

    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
    std::env::remove_var("MOCK_CREATE_MS");
    std::env::remove_var("MOCK_EXEC_MS");
}

/// A source file larger than the per-file cap is embedded cut, with an
/// explicit `[truncated after N bytes]` marker — never silently dropped.
#[tokio::test(flavor = "multi_thread")]
async fn gather_validator_evidence_truncates_large_sources_with_marker() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state4.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let sprite = SpriteClient::new(SpriteConfig {
        cli_path: option_env!("CARGO_BIN_EXE_mock-sprite-cli")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/debug/mock-sprite-cli")
            }),
        command_timeout: Duration::from_secs(15),
        ..SpriteConfig::default()
    });
    sprite.create("ev-sprite4").await.unwrap();
    sprite.set_url_auth_public("ev-sprite4").await.unwrap();

    let workdir = dir.path().join("swarm-work").join("lane-3");
    std::fs::create_dir_all(workdir.join("src")).unwrap();
    std::fs::write(
        workdir.join("index.html"),
        "<html><body>game</body></html>\n",
    )
    .unwrap();
    let big = "BIGMAIN-MARKER ".repeat(40 * 1024 / 15);
    std::fs::write(workdir.join("src/main.ts"), big).unwrap();

    let evidence = gather_validator_evidence(
        &sprite,
        "ev-sprite4",
        workdir.to_str().unwrap(),
        "https://203.0.113.1/nope",
        false,
    )
    .await;

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

    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
    std::env::remove_var("MOCK_CREATE_MS");
    std::env::remove_var("MOCK_EXEC_MS");
}

/// Best-effort: an unreachable public URL yields an explicit probe-failure
/// block instead of failing the validator turn.
#[tokio::test(flavor = "multi_thread")]
async fn gather_validator_evidence_reports_unreachable_probe_as_data() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state2.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let sprite = SpriteClient::new(SpriteConfig {
        cli_path: option_env!("CARGO_BIN_EXE_mock-sprite-cli")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/debug/mock-sprite-cli")
            }),
        command_timeout: Duration::from_secs(15),
        ..SpriteConfig::default()
    });
    sprite.create("ev-sprite2").await.unwrap();
    sprite.set_url_auth_public("ev-sprite2").await.unwrap();

    // Empty workdir: the file list exec still succeeds (find on an empty dir).
    let workdir = dir.path().join("swarm-work").join("lane-1");
    std::fs::create_dir_all(&workdir).unwrap();

    // Unroutable documentation-range host: resolution/connect fails fast.
    let evidence = gather_validator_evidence(
        &sprite,
        "ev-sprite2",
        workdir.to_str().unwrap(),
        "https://203.0.113.1/nope",
        false,
    )
    .await;

    assert!(
        evidence.contains("<data label=\"http-probe\">"),
        "evidence: {evidence}"
    );
    assert!(
        evidence.contains("GET https://203.0.113.1/nope failed"),
        "the probe failure must be stated in DATA: {evidence}"
    );

    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
    std::env::remove_var("MOCK_CREATE_MS");
    std::env::remove_var("MOCK_EXEC_MS");
}
