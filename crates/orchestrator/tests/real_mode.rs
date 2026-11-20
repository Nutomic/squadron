//! Real-mode (`test_mode = false`) integration test — the seam where audits
//! #1–#3 escaped: it drives the full pipeline against the mock sprite CLI and
//! proves that (a) the lane URL is derived from `sprite info`, (b) the URL
//! auth is flipped public, (c) the 200 gate passes against a live local
//! (tunneled) server, and (d) the artifact pull runs from the sprite — all
//! together in one run.
//!
//! This file intentionally contains a single test: it mutates process env
//! (PATH, cwd, MOCK_* vars) and must not race with a sibling test.
#![allow(clippy::unwrap_used, clippy::panic)] // test code: panics on failure are the assertion mechanism

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use axum::Router;
use flate2::read::GzDecoder;
use orchestrator::{
    HarnessMode, HubRegistry, PipelineConfig, PipelineCtx, RunRequest, ScreenshotQueue,
    SlotSemaphore, SseHub,
};
use sprites_client::{SpriteClient, SpriteConfig};
use store::Store;
use tokio::sync::RwLock;

fn canned_impl() -> String {
    serde_json::json!({
        "summary": "Implemented the tunneled page",
        "files": 1,
        "run_command": "python3 -m http.server 8080",
        "done": true,
        "blockers": []
    })
    .to_string()
}

fn canned_val() -> String {
    serde_json::json!({
        "verdict": "pass",
        "features": ["tunnel page"],
        "bugs": [],
        "security": "ok",
        "quality_score": 90.0,
        "summary": "Detailed validation: the live URL answers and content matches."
    })
    .to_string()
}

fn shell_quote(json: &str) -> String {
    json.replace('\'', "'\\''")
}

fn short_run_id(run_id: &str) -> String {
    run_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn real_mode_url_auth_200_gate_and_artifact_pull_run_together() {
    let project = tempfile::tempdir().unwrap();
    let old_cwd = std::env::current_dir().unwrap();
    // The mock CLI executes commands in this process' cwd; the sprite-side
    // workdir (`swarm-work/...`) is relative, so run from a private cwd.
    std::env::set_current_dir(project.path()).unwrap();

    // Fake `tokei` on PATH so the sprite-side tokei step of the artifact pull
    // produces totals on a box without the real binary.
    let bin_dir = project.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let tokei = bin_dir.join("tokei");
    std::fs::write(
        &tokei,
        "#!/bin/sh\necho '{\"Total\":{\"code\":42},\"HTML\":{\"reports\":[1,2,3]}}'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tokei, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let old_path = std::env::var_os("PATH");
    std::env::set_var(
        "PATH",
        format!(
            "{}:{}",
            bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );

    // The "tunnel": a local server every sprite URL points at, answering 200.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().fallback(get(|| async { "<html><body>tunnel</body></html>" }));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    std::env::set_var(
        "MOCK_SPRITE_STATE",
        project.path().join("mock-sprites.json"),
    );
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    // Slow the impl exec down so the sprite workdir can be populated right
    // after start_run returns (the pull happens after the impl session).
    std::env::set_var("MOCK_EXEC_MS", "1200");
    // `sprite info` reports the tunneled URL for every sprite.
    std::env::set_var("MOCK_URL_TEMPLATE", format!("http://{addr}/{{name}}"));
    // Real mode must never depend on a local chromium being present.
    std::env::remove_var("CHROMIUM_BIN");

    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();

    let sprite = SpriteClient::new(SpriteConfig {
        cli_path: option_env!("CARGO_BIN_EXE_mock-sprite-cli")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mock-sprite-cli")
            }),
        command_timeout: Duration::from_secs(15),
        ..SpriteConfig::default()
    });

    let config = PipelineConfig {
        n_lanes: 3,
        max_running_slots: 10,
        retries_per_lane: 0,
        test_mode: false, // REAL MODE
        data_dir: project.path().join("data"),
        impl_deadline: Duration::from_secs(30),
        val_deadline: Duration::from_secs(30),
        judge_deadline: Duration::from_secs(15),
        ..PipelineConfig::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();

    let screenshots = ScreenshotQueue::with_data_dir(store.clone(), false, config.data_dir.clone());
    let slots = SlotSemaphore::new(config.max_running_slots);
    let hub_registry: HubRegistry = Arc::new(RwLock::new(HashMap::new()));
    let ctx = Arc::new(PipelineCtx {
        store: store.clone(),
        sprite,
        machine_llm: None, // fallback judge
        sse: SseHub::new(256),
        screenshots,
        config,
        slots,
        harness_cache: Arc::new(orchestrator::harness::HarnessCache::new()),
        hub_registry: Some(hub_registry.clone()),
    });

    let harness = HarnessMode::Command {
        impl_command: format!(
            "echo '<h1>tunnel page</h1>' > index.html; echo '{}'",
            shell_quote(&canned_impl())
        ),
        val_command: format!("echo '{}'", shell_quote(&canned_val())),
    };

    let run = orchestrator::start_run(
        ctx.clone(),
        RunRequest {
            brief: "Build a small tunneled site".to_owned(),
            n_lanes: 3,
            harness,
        },
    )
    .await
    .unwrap();

    // Populate each lane's sprite-side workdir while the (slow) impl exec is
    // still running, so the artifact pull has real content to bring back.
    let short = short_run_id(&run.id);
    for idx in 0..3i64 {
        let dir = project
            .path()
            .join("swarm-work")
            .join(&short)
            .join(format!("lane-{idx}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), "<h1>pulled from sprite</h1>\n").unwrap();
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        let r = store.get_run(&run.id).await.unwrap().unwrap();
        if r.status == "judged" || r.status == "failed" {
            if r.status != "judged" {
                let lanes = store.list_lanes(&run.id).await.unwrap();
                panic!(
                    "real-mode run should be judged: status={} lanes={:?}",
                    r.status,
                    lanes
                        .iter()
                        .map(|l| (l.idx, &l.state, &l.error))
                        .collect::<Vec<_>>()
                );
            }
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("timeout waiting for the real-mode run");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // (b) public auth was flipped on through the CLI.
    let state_json = std::fs::read_to_string(project.path().join("mock-sprites.json")).unwrap();
    let state: sprites_client::mock_cli::MockState = serde_json::from_str(&state_json).unwrap();
    assert!(
        state.url_auth_public,
        "url-auth must be public in real mode"
    );

    let lanes = store.list_lanes(&run.id).await.unwrap();
    assert_eq!(lanes.len(), 3);
    for lane in &lanes {
        // (a) URL derivation: the recorded lane URL comes from `sprite info`
        // (the tunneled local URL), not a hardcoded guess.
        let url = lane.impl_url.clone().expect("real mode records impl_url");
        assert!(
            url.starts_with(&format!("http://{addr}/"))
                && url.ends_with(&format!("impl-{}", lane.idx)),
            "impl_url must be derived from `sprite info`, got: {url}"
        );
        assert_eq!(
            lane.state, "validated",
            "lane {} err={:?}",
            lane.idx, lane.error
        );
        assert_eq!(lane.val_verdict.as_deref(), Some("pass"));

        // (d) artifact pull ran and brought the sprite workdir back.
        let tarball = lane
            .tarball_path
            .clone()
            .expect("tarball pulled from sprite");
        assert!(!tarball.is_empty());
        let bytes = tokio::fs::read(&tarball).await.unwrap();
        let mut archive = tar::Archive::new(GzDecoder::new(&bytes[..]));
        let mut found = false;
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap().ends_with("index.html") {
                let mut content = String::new();
                std::io::Read::read_to_string(&mut entry, &mut content).unwrap();
                assert!(
                    content.contains("pulled from sprite"),
                    "tarball must contain the sprite workdir content: {content}"
                );
                found = true;
            }
        }
        assert!(found, "artifact pull must include index.html");
        let tokei = lane.tokei_json.clone().unwrap_or_default();
        let (files, loc) = orchestrator::artifacts::parse_tokei_totals(&tokei);
        assert_eq!(files, Some(3), "sprite-side tokei totals parsed: {tokei}");
        assert_eq!(loc, Some(42));
        assert!(lane.console_tail.is_some(), "console tail recorded");
    }

    // (c) the 200 gate: every validated lane's URL answered 200 on the tunnel
    // (implied by reaching `validated` in real mode — the gate is not skipped
    // when test_mode is off).
    assert!(
        lanes.iter().all(|l| l.state == "validated"),
        "the 200 gate passed for every lane"
    );

    // Audit R3 finding 5: the terminal run removed its hub from the registry.
    assert!(
        hub_registry.read().await.is_empty(),
        "hub registry must not leak the terminal run's channel"
    );

    // Cleanup: restore process-global state.
    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
    std::env::remove_var("MOCK_CREATE_MS");
    std::env::remove_var("MOCK_EXEC_MS");
    std::env::remove_var("MOCK_URL_TEMPLATE");
    match old_path {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }
    std::env::set_current_dir(old_cwd).unwrap();
}
