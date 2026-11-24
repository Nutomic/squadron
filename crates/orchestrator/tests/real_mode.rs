//! Real-mode (`test_mode = false`) integration test — the seam where audits
//! #1–#3 escaped: it drives the full pipeline against the mock sprite CLI and
//! proves that (a) the lane URL is derived from `sprite info`, (b) the URL
//! auth is flipped public (and re-issued when the 200-check hits an auth
//! wall), (c) the 200 gate passes against a live local (tunneled) server, and
//! (d) the artifact pull runs from the sprite — all together in one run.
//!
//! The tests mutate process env (PATH, cwd, MOCK_* vars) and must not
//! race with each other; ENV_LOCK serializes them.
#![allow(clippy::unwrap_used, clippy::panic)] // test code: panics on failure are the assertion mechanism

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use flate2::read::GzDecoder;
use orchestrator::{
    HarnessMode, HubRegistry, PipelineConfig, PipelineCtx, RunRequest, ScreenshotQueue,
    SlotSemaphore, SseHub,
};

mod common;

use common::wait_secs;
use sprites_client::{SpriteClient, SpriteConfig};
use store::Store;
use tokio::sync::{Mutex, RwLock};

// Both tests mutate process-global state (PATH, cwd, MOCK_* env): serialize.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    let _env = ENV_LOCK.lock().await;
    let project = tempfile::tempdir().unwrap();
    // F6: a sibling test that panicked before its cleanup leaves the process
    // cwd inside a deleted tempdir; fall back to a stable dir instead of
    // compounding the failure with a NotFound on current_dir().
    let old_cwd = std::env::current_dir()
        .ok()
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    // The mock CLI executes commands in this process' cwd; the sprite-side
    // workdir (`squadron-work/...`) is relative, so run from a private cwd.
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

    // The "tunnel": a local server every sprite URL points at. F6: the FIRST
    // probe of each lane URL answers with the platform auth-wall signature
    // (403); only after the pipeline re-issues `config update --url-auth
    // public` and re-probes does the URL answer 200. The run must therefore
    // survive one auth-wall round per lane instead of dying on it.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let probes: Arc<Mutex<HashMap<String, u32>>> = Arc::new(Mutex::new(HashMap::new()));
    let app = Router::new().fallback(get(move |req: Request| {
        let probes = probes.clone();
        async move {
            let mut probes = probes.lock().await;
            let seen = probes.entry(req.uri().path().to_owned()).or_insert(0);
            *seen += 1;
            if *seen == 1 {
                (StatusCode::FORBIDDEN, "auth wall")
            } else {
                (StatusCode::OK, "<html><body>tunnel</body></html>")
            }
        }
    }));
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
        // F2 test-only escape: the tunnel URL is loopback.
        probe_allow_loopback: true,
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
        lane_queue: None,
        create_throttle: std::sync::Arc::new(orchestrator::SpriteCreateThrottle::disabled()),
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
            .join("squadron-work")
            .join(&short)
            .join(format!("lane-{idx}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), "<h1>pulled from sprite</h1>\n").unwrap();
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs());
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

    // (b2) review F5 ordering regression: for EVERY production lane sprite
    // the `sprite config update --url-auth public` command is issued after
    // `create` and BEFORE the implementer session (the first `exec`) —
    // otherwise the lane URL stays private until it is too late. (Lanes run
    // concurrently, so commands of different sprites interleave; the
    // ordering is per sprite.)
    for name in state.sprites.keys() {
        let log = &state.command_log;
        let create_pos = log
            .iter()
            .position(|e| e == &format!("create {name}"))
            .expect("lane sprite was created through the mock CLI");
        let auth_pos = log
            .iter()
            .position(|e| e == &format!("config {name}"))
            .unwrap_or_else(|| {
                panic!(
                    "url-auth public must be issued for every lane sprite ({name}); log: {log:?}"
                )
            });
        assert!(
            auth_pos > create_pos,
            "url-auth public must follow create for {name}; log: {log:?}"
        );
        let first_exec = log
            .iter()
            .position(|e| e == &format!("exec {name}"))
            .expect("the implementer session runs via `sprite exec`");
        assert!(
            auth_pos < first_exec,
            "url-auth public must precede the implementer session for {name}"
        );
        // F6: every lane's first 200-probe hits the tunnel's 403 auth wall,
        // so the pipeline must re-issue the config update exactly once more.
        let config_calls = log
            .iter()
            .filter(|e| *e == &format!("config {name}"))
            .count();
        assert_eq!(
            config_calls, 2,
            "initial + auth-wall retry config update expected for {name}"
        );
        // F7: the config update was VERIFIED via `sprite info` before the
        // first exec — the parsed `Auth:` value decides, not the update's
        // own output. The first `info <name>` verification call must land
        // after the first config update and before the implementer session.
        let info_pos = log
            .iter()
            .position(|e| e == &format!("info {name}"))
            .unwrap_or_else(|| panic!("sprite info verification never ran for {name}"));
        assert!(
            info_pos > auth_pos && info_pos < first_exec,
            "sprite info must verify auth=public between the config update and the first exec for {name}; log: {log:?}"
        );
    }

    // F7: each lane's event log carries the parsed-Auth verification line.
    for idx in 0..3i64 {
        let events = store.list_lane_events(&run.id, idx).await.unwrap();
        assert!(
            events.iter().any(|ev| {
                ev.kind == "log"
                    && ev
                        .payload
                        .contains("url-auth public verified via sprite info")
            }),
            "lane {idx} must log the sprite info auth verification: {events:?}"
        );
    }

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
    // `drop_hub` runs in the spawned pipeline task after the DB status flips
    // terminal — poll until it drains before asserting (CI drain-race).
    common::wait_hub_drained(&hub_registry, &run.id).await;
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

/// F6 regression: a PERSISTENT auth wall (the retry cannot fix it — routing
/// still answers 403 after the re-issued config update and the settle delay)
/// must fail the lane with the post-retry error, not loop or pass.
#[tokio::test(flavor = "multi_thread")]
async fn persistent_auth_wall_fails_the_lane_after_one_retry() {
    let _env = ENV_LOCK.lock().await;
    let project = tempfile::tempdir().unwrap();
    let old_cwd = std::env::current_dir()
        .ok()
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    std::env::set_current_dir(project.path()).unwrap();

    // A tunnel that ALWAYS answers the auth-wall signature.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().fallback(get(|| async { (StatusCode::FORBIDDEN, "auth wall") }));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    std::env::set_var(
        "MOCK_SPRITE_STATE",
        project.path().join("mock-sprites.json"),
    );
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");
    std::env::set_var("MOCK_URL_TEMPLATE", format!("http://{addr}/{{name}}"));
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
        n_lanes: 2,
        max_running_slots: 10,
        retries_per_lane: 0,
        test_mode: false,
        data_dir: project.path().join("data"),
        impl_deadline: Duration::from_secs(30),
        val_deadline: Duration::from_secs(30),
        judge_deadline: Duration::from_secs(15),
        probe_allow_loopback: true,
        ..PipelineConfig::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();

    let screenshots = ScreenshotQueue::with_data_dir(store.clone(), false, config.data_dir.clone());
    let slots = SlotSemaphore::new(config.max_running_slots);
    let ctx = Arc::new(PipelineCtx {
        store: store.clone(),
        sprite,
        machine_llm: None,
        sse: SseHub::new(256),
        screenshots,
        config,
        slots,
        harness_cache: Arc::new(orchestrator::harness::HarnessCache::new()),
        hub_registry: None,
        lane_queue: None,
        create_throttle: std::sync::Arc::new(orchestrator::SpriteCreateThrottle::disabled()),
    });

    let harness = HarnessMode::Command {
        impl_command: format!("echo site; echo '{}'", shell_quote(&canned_impl())),
        val_command: format!("echo '{}'", shell_quote(&canned_val())),
    };

    let run = orchestrator::start_run(
        ctx,
        RunRequest {
            brief: "Build a small site".to_owned(),
            n_lanes: 2,
            harness,
        },
    )
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs());
    loop {
        let r = store.get_run(&run.id).await.unwrap().unwrap();
        if r.status == "judged" || r.status == "failed" {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("timeout waiting for the real-mode run to fail");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Every lane died at the 200 gate — after exactly one auth-wall retry.
    let lanes = store.list_lanes(&run.id).await.unwrap();
    assert_eq!(lanes.len(), 2);
    for lane in &lanes {
        let err = lane.error.clone().unwrap_or_default();
        assert!(
            err.contains("did not answer 200 after url-auth retry"),
            "lane {} must fail at the 200 gate after the retry: state={} err={err:?}",
            lane.idx,
            lane.state
        );
    }

    // The retry re-issued the config update exactly once per sprite.
    let state_json = std::fs::read_to_string(project.path().join("mock-sprites.json")).unwrap();
    let state: sprites_client::mock_cli::MockState = serde_json::from_str(&state_json).unwrap();
    for name in state.sprites.keys() {
        let config_calls = state
            .command_log
            .iter()
            .filter(|e| *e == &format!("config {name}"))
            .count();
        assert_eq!(
            config_calls, 2,
            "the auth-wall retry must stay bounded (one re-issue) for {name}"
        );
    }

    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
    std::env::remove_var("MOCK_CREATE_MS");
    std::env::remove_var("MOCK_EXEC_MS");
    std::env::remove_var("MOCK_URL_TEMPLATE");
    std::env::set_current_dir(old_cwd).unwrap();
}

/// F7 regression (a): when `sprite config update --url-auth public` keeps
/// failing, the lane must fail EARLY — before the implementer session is
/// launched — with a clear error naming the config failure, and every retry
/// attempt's output must be visible in lane_events. No implementer hour may
/// be spent on a sprite that can never pass the 200-check.
#[tokio::test(flavor = "multi_thread")]
async fn failing_config_update_fails_the_lane_before_any_exec() {
    let _env = ENV_LOCK.lock().await;
    let project = tempfile::tempdir().unwrap();
    let old_cwd = std::env::current_dir()
        .ok()
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    std::env::set_current_dir(project.path()).unwrap();

    std::env::set_var(
        "MOCK_SPRITE_STATE",
        project.path().join("mock-sprites.json"),
    );
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");
    // Every `sprite config update` fails with a non-zero exit: the retry
    // loop must exhaust its 3 attempts (each logged) and then fail the lane.
    std::env::set_var("MOCK_CONFIG_FAIL", "1");
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
        n_lanes: 2,
        max_running_slots: 10,
        retries_per_lane: 0,
        test_mode: false,
        data_dir: project.path().join("data"),
        impl_deadline: Duration::from_secs(30),
        val_deadline: Duration::from_secs(30),
        judge_deadline: Duration::from_secs(15),
        probe_allow_loopback: true,
        ..PipelineConfig::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();

    let screenshots = ScreenshotQueue::with_data_dir(store.clone(), false, config.data_dir.clone());
    let slots = SlotSemaphore::new(config.max_running_slots);
    let ctx = Arc::new(PipelineCtx {
        store: store.clone(),
        sprite,
        machine_llm: None,
        sse: SseHub::new(256),
        screenshots,
        config,
        slots,
        harness_cache: Arc::new(orchestrator::harness::HarnessCache::new()),
        hub_registry: None,
        lane_queue: None,
        create_throttle: std::sync::Arc::new(orchestrator::SpriteCreateThrottle::disabled()),
    });

    let harness = HarnessMode::Command {
        impl_command: format!("echo site; echo '{}'", shell_quote(&canned_impl())),
        val_command: format!("echo '{}'", shell_quote(&canned_val())),
    };

    let run = orchestrator::start_run(
        ctx,
        RunRequest {
            brief: "Build a small site".to_owned(),
            n_lanes: 2,
            harness,
        },
    )
    .await
    .unwrap();

    // The run must reach a terminal state well within the deadline: the lane
    // fails fast pre-session (3 config attempts + 1s/3s backoff), not after
    // an implementer hour.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs());
    loop {
        let r = store.get_run(&run.id).await.unwrap().unwrap();
        if r.status == "judged" || r.status == "failed" {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("timeout waiting for the run to fail on the config update");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let lanes = store.list_lanes(&run.id).await.unwrap();
    assert_eq!(lanes.len(), 2);
    for lane in &lanes {
        assert_eq!(lane.state, "failed", "lane must fail early: {lane:?}");
        let err = lane.error.clone().unwrap_or_default();
        assert!(
            err.contains("url-auth"),
            "the lane error must name the url-auth config failure: {err:?}"
        );

        // Every attempt was logged to lane_events (sanitized output capture).
        let events = store.list_lane_events(&run.id, lane.idx).await.unwrap();
        let attempt_logs = events
            .iter()
            .filter(|ev| ev.kind == "log" && ev.payload.contains("url-auth config update attempt"))
            .count();
        assert_eq!(
            attempt_logs, 3,
            "all 3 config update attempts must be logged: {events:?}"
        );
    }

    // The implementer session was never launched: no exec ran at all.
    let state_json = std::fs::read_to_string(project.path().join("mock-sprites.json")).unwrap();
    let state: sprites_client::mock_cli::MockState = serde_json::from_str(&state_json).unwrap();
    assert!(
        state.command_log.iter().all(|e| !e.starts_with("exec ")),
        "no sprite exec may run before the url-auth config is verified; log: {:?}",
        state.command_log
    );
    // The lane sprite was destroyed after the early failure (billable
    // resources must not leak).
    assert!(
        state.sprites.is_empty(),
        "the failed lane sprite must be destroyed: {:?}",
        state.sprites.keys().collect::<Vec<_>>()
    );
    // The config update was retried 3 times (bounded) for the lane sprite.
    let seg: String = short_run_id(&run.id)
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .collect();
    for idx in 0..2 {
        let name = format!("squadron-{seg}-impl-{idx}");
        let config_calls = state
            .command_log
            .iter()
            .filter(|e| *e == &format!("config {name}"))
            .count();
        assert_eq!(
            config_calls, 3,
            "the config update must be retried 3 times (bounded) for {name}"
        );
    }

    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
    std::env::remove_var("MOCK_CREATE_MS");
    std::env::remove_var("MOCK_EXEC_MS");
    std::env::remove_var("MOCK_CONFIG_FAIL");
    std::env::set_current_dir(old_cwd).unwrap();
}

/// F7 regression (b): the 200-check auth-wall retry must VERIFY the re-issued
/// config update through `sprite info` (parsed `Auth:` value) before
/// re-probing — and pass when routing actually became public.
#[tokio::test(flavor = "multi_thread")]
async fn auth_wall_retry_verifies_public_before_reprobe() {
    let _env = ENV_LOCK.lock().await;
    let project = tempfile::tempdir().unwrap();
    let old_cwd = std::env::current_dir()
        .ok()
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    std::env::set_current_dir(project.path()).unwrap();

    // The wall answers 403 exactly once per path; the re-probe (after the
    // asserted re-issue) sees 200.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let probes: Arc<Mutex<HashMap<String, u32>>> = Arc::new(Mutex::new(HashMap::new()));
    let app = Router::new().fallback(get(move |req: Request| {
        let probes = probes.clone();
        async move {
            let mut probes = probes.lock().await;
            let seen = probes.entry(req.uri().path().to_owned()).or_insert(0);
            *seen += 1;
            if *seen == 1 {
                (StatusCode::FORBIDDEN, "auth wall")
            } else {
                (StatusCode::OK, "<html><body>tunnel</body></html>")
            }
        }
    }));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    std::env::set_var(
        "MOCK_SPRITE_STATE",
        project.path().join("mock-sprites.json"),
    );
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");
    std::env::set_var("MOCK_URL_TEMPLATE", format!("http://{addr}/{{name}}"));
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
        n_lanes: 2,
        max_running_slots: 10,
        retries_per_lane: 0,
        test_mode: false,
        data_dir: project.path().join("data"),
        impl_deadline: Duration::from_secs(30),
        val_deadline: Duration::from_secs(30),
        judge_deadline: Duration::from_secs(15),
        probe_allow_loopback: true,
        ..PipelineConfig::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();

    let screenshots = ScreenshotQueue::with_data_dir(store.clone(), false, config.data_dir.clone());
    let slots = SlotSemaphore::new(config.max_running_slots);
    let ctx = Arc::new(PipelineCtx {
        store: store.clone(),
        sprite,
        machine_llm: None,
        sse: SseHub::new(256),
        screenshots,
        config,
        slots,
        harness_cache: Arc::new(orchestrator::harness::HarnessCache::new()),
        hub_registry: None,
        lane_queue: None,
        create_throttle: std::sync::Arc::new(orchestrator::SpriteCreateThrottle::disabled()),
    });

    let harness = HarnessMode::Command {
        impl_command: format!("echo site; echo '{}'", shell_quote(&canned_impl())),
        val_command: format!("echo '{}'", shell_quote(&canned_val())),
    };

    let run = orchestrator::start_run(
        ctx,
        RunRequest {
            brief: "Build a small tunneled site".to_owned(),
            n_lanes: 2,
            harness,
        },
    )
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs());
    loop {
        let r = store.get_run(&run.id).await.unwrap().unwrap();
        if r.status == "judged" || r.status == "failed" {
            if r.status != "judged" {
                let lanes = store.list_lanes(&run.id).await.unwrap();
                panic!(
                    "the auth-wall retry must recover the lane: status={} lanes={:?}",
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

    let lanes = store.list_lanes(&run.id).await.unwrap();
    assert_eq!(lanes.len(), 2);
    for lane in &lanes {
        assert_eq!(
            lane.state, "validated",
            "lane {} must pass after the retry: err={:?}",
            lane.idx, lane.error
        );
    }

    // The event log shows the ordered recovery: auth wall -> re-issued config
    // update -> sprite info verification (parsed auth=public) -> pass.
    for idx in 0..2i64 {
        let events = store.list_lane_events(&run.id, idx).await.unwrap();
        let wall_pos = events
            .iter()
            .position(|ev| ev.kind == "log" && ev.payload.contains("hit an auth wall"));
        let verified_pos = events.iter().rposition(|ev| {
            ev.kind == "log"
                && ev
                    .payload
                    .contains("url-auth public verified via sprite info")
        });
        let (wall_pos, verified_pos) = match (wall_pos, verified_pos) {
            (Some(w), Some(v)) => (w, v),
            _ => panic!("lane {idx}: auth-wall + verification logs expected: {events:?}"),
        };
        assert!(
            verified_pos > wall_pos,
            "lane {idx}: the sprite info verification must follow the auth wall: {events:?}"
        );
        // Two verification logs: one pre-session, one in the retry path.
        let verified_count = events
            .iter()
            .filter(|ev| {
                ev.kind == "log"
                    && ev
                        .payload
                        .contains("url-auth public verified via sprite info")
            })
            .count();
        assert_eq!(
            verified_count, 2,
            "lane {idx}: verification must run in BOTH paths (pre-session + retry): {events:?}"
        );
    }

    // The re-issue happened exactly once more (initial + retry).
    let state_json = std::fs::read_to_string(project.path().join("mock-sprites.json")).unwrap();
    let state: sprites_client::mock_cli::MockState = serde_json::from_str(&state_json).unwrap();
    for name in state.sprites.keys() {
        let config_calls = state
            .command_log
            .iter()
            .filter(|e| *e == &format!("config {name}"))
            .count();
        assert_eq!(
            config_calls, 2,
            "initial + auth-wall retry config update expected for {name}"
        );
    }

    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
    std::env::remove_var("MOCK_CREATE_MS");
    std::env::remove_var("MOCK_EXEC_MS");
    std::env::remove_var("MOCK_URL_TEMPLATE");
    std::env::set_current_dir(old_cwd).unwrap();
}
