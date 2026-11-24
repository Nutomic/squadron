#![allow(clippy::unwrap_used, clippy::panic)] // test code: panics on failure are the assertion mechanism

//! F24: the confirm path must DRIVE the pipeline.
//!
//! The prod bug: the dialog-confirm handler only persisted run + lane rows
//! (`persist_run_from_cases`) and returned — nothing ever started the
//! pipeline or enqueued the run, so confirmed runs sat `queued` forever.
//!
//! These tests cover the fixed behavior against the fake (mock CLI) sprites:
//! - submitted cases reach `judged` through the same machinery as a
//!   decomposed run (`start_run_cases`, exercised via the state API because
//!   the web handler always uses the real harness, which the mock cannot
//!   answer);
//! - the web confirm handler itself flips a fitting run to `running` (before
//!   the fix it stayed `queued` forever);
//! - a run whose lanes do not fit stays `queued`, and after a simulated
//!   restart (fresh AppState + `pickup_queued_runs`) it is re-enqueued,
//!   promoted FIFO, and executed to `judged` (crash-safe queue).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use orchestrator::{HarnessMode, PipelineConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

use sprites_client::{SpriteClient, SpriteConfig};
use squadron::{router, AppState};
use store::Store;

const STATIC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/static");

const MOCK_CONNECT_INFO: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 1234);

/// Serializes the mock-CLI env vars across tests in this binary.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn canned_impl() -> String {
    serde_json::json!({
        "summary": "Implemented the submitted case",
        "files": 3,
        "run_command": "python3 -m http.server 8080",
        "done": true,
        "blockers": []
    })
    .to_string()
}

fn canned_val() -> String {
    serde_json::json!({
        "verdict": "pass",
        "features": ["the case prompt"],
        "bugs": [],
        "security": "no issues found",
        "quality_score": 90.0,
        "summary": "Detailed validation: implementation matches the submitted case and serves content."
    })
    .to_string()
}

/// Test-only canned-command harness (same trick as the orchestrator queue
/// tests): each impl session prints a schema-valid ImplementerOutput, each
/// validation session a passing ValidatorOutput.
fn harness_for(slow_prefix: &str) -> HarnessMode {
    let prefix = if slow_prefix.is_empty() {
        String::new()
    } else {
        format!("{slow_prefix}; ")
    };
    HarnessMode::Command {
        impl_command: format!(
            "{prefix}echo '<h1>hi</h1>' > index.html; echo '{}'",
            canned_impl().replace('\'', "'\\''")
        ),
        val_command: format!("echo '{}'", canned_val().replace('\'', "'\\''")),
    }
}

fn mock_cli_path() -> PathBuf {
    option_env!("CARGO_BIN_EXE_mock-sprite-cli")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mock-sprite-cli")
        })
}

fn base_config(dir: &std::path::Path, max_running_slots: usize) -> PipelineConfig {
    PipelineConfig {
        n_lanes: 3,
        max_running_slots,
        test_mode: true,
        data_dir: dir.join("data"),
        impl_deadline: Duration::from_secs(30),
        val_deadline: Duration::from_secs(30),
        judge_deadline: Duration::from_secs(15),
        ..PipelineConfig::default()
    }
}

struct TestEnv {
    state: AppState,
    store: Store,
    _tmp: tempfile::TempDir,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

/// A pipeline-ENABLED AppState on the mock sprite CLI (no `.without_pipeline`):
/// exactly the production wiring, with fakes behind the sprite boundary.
async fn setup(max_running_slots: usize) -> TestEnv {
    let guard = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-sprites.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    let sprite = SpriteClient::new(SpriteConfig {
        cli_path: mock_cli_path(),
        command_timeout: Duration::from_secs(10),
        ..SpriteConfig::default()
    });
    let config = base_config(dir.path(), max_running_slots);
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state =
        AppState::new(store.clone(), sprite, config).with_static_dir(PathBuf::from(STATIC_DIR));
    TestEnv {
        state,
        store,
        _tmp: dir,
        _guard: guard,
    }
}

/// A second AppState over the SAME store + mock state: a simulated process
/// restart (fresh in-memory queue, fresh slot pool, same DB rows). The
/// pipeline harness is overridden to the test-only canned command mode — in
/// production the pickup resumes with the state's real harness; the harness
/// mode itself is not persisted, so a picked-up run can only ever resume
/// with the (new) state's own harness.
fn restart(env: &TestEnv, config: PipelineConfig, harness: HarnessMode) -> AppState {
    let sprite = SpriteClient::new(SpriteConfig {
        cli_path: mock_cli_path(),
        command_timeout: Duration::from_secs(10),
        ..SpriteConfig::default()
    });
    std::fs::create_dir_all(&config.data_dir).unwrap();
    AppState::new(env.store.clone(), sprite, config)
        .with_static_dir(PathBuf::from(STATIC_DIR))
        .with_pipeline_harness(harness)
}

async fn wait_run_status(store: &Store, run_id: &str, statuses: &[&str], secs: u64) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let r = store.get_run(run_id).await.unwrap().unwrap();
        if statuses.contains(&r.status.as_str()) {
            return r.status;
        }
        if tokio::time::Instant::now() > deadline {
            panic!(
                "timeout waiting for run {run_id} status in {statuses:?}; last={}",
                r.status
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn submitted_cases() -> Vec<Value> {
    vec![
        json!({"label": "emerald", "impl_prompt": "emerald edition, heavily edited"}),
        json!({"label": "crimson", "impl_prompt": "crimson edition, also edited"}),
        json!({"label": "azure", "impl_prompt": "azure edition of the brief"}),
    ]
}

/// (a) The full pipeline test via the state API: cases submitted through the
/// confirm path EXECUTE — the run reaches `judged`, its lanes validate, and
/// each lane's variant_json carries the submitted (edited) prompt.
#[tokio::test(flavor = "multi_thread")]
async fn confirm_submitted_cases_execute_to_judged() {
    let env = setup(8).await;
    let cases = submitted_cases();
    let run = env
        .state
        .start_pipeline_cases_with_harness(
            "a green or red product site".to_owned(),
            cases.clone(),
            harness_for(""),
        )
        .await
        .unwrap();
    assert_eq!(run.n_lanes, 3);
    assert_eq!(run.status, "running", "lanes that fit must start");

    let status = wait_run_status(&env.store, &run.id, &["judged", "failed"], 60).await;
    assert_eq!(status, "judged", "submitted cases must run to the judge");
    let final_run = env.store.get_run(&run.id).await.unwrap().unwrap();
    assert!(final_run.judge_verdict.is_some(), "a judge verdict exists");
    let lanes = env.store.list_lanes(&run.id).await.unwrap();
    assert_eq!(lanes.len(), 3);
    for lane in &lanes {
        assert_eq!(lane.state, "validated", "lane {} must validate", lane.idx);
    }
    let prompts: Vec<String> = lanes
        .iter()
        .map(|l| {
            let v: Value = serde_json::from_str(&l.variant_json).unwrap();
            v["impl_prompt"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(prompts[0], "emerald edition, heavily edited");
    assert_eq!(prompts[1], "crimson edition, also edited");
    assert_eq!(prompts[2], "azure edition of the brief");
}

/// (a, web level) The confirm handler itself must start execution: a fitting
/// submission is `running` in the store by the time the 201 is returned.
/// (The bug: it stayed `queued` forever with no dispatcher activity.)
#[tokio::test(flavor = "multi_thread")]
async fn web_confirm_starts_execution() {
    let env = setup(8).await;
    let app = router(Arc::new(env.state.clone())).layer(
        axum::extract::connect_info::MockConnectInfo(MOCK_CONNECT_INFO),
    );

    // GET / issues the double-submit CSRF token.
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let token = res
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .expect("GET / must set squadron_csrf")
        .strip_prefix("squadron_csrf=")
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();

    let body = json!({
        "brief": "a tiny site",
        "cases": [{"label": "solo", "impl_prompt": "only worker"}],
        "csrf_token": token,
    });
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/runs")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("squadron_csrf={token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let bytes = http_body_util::BodyExt::collect(res.into_body())
        .await
        .unwrap()
        .to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["n_lanes"], json!(1));

    // Execution started: the run is NOT stranded in `queued`.
    let run_id = value["run_id"].as_str().unwrap().to_owned();
    let run = env.store.get_run(&run_id).await.unwrap().unwrap();
    assert_eq!(
        run.status, "running",
        "confirm must drive the pipeline, not just persist rows"
    );
}

/// (b) Lanes that don't fit → the run is enqueued (status `queued`), and
/// after a simulated restart (fresh AppState + `pickup_queued_runs`) the
/// stranded DB-queued run is re-enqueued, FIFO-promoted, and executed.
#[tokio::test(flavor = "multi_thread")]
async fn queued_run_is_picked_up_and_promoted_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config = base_config(dir.path(), 3);
    let env = setup(3).await;

    // Fill the whole 3-slot pool with a slow run so the subject cannot fit.
    let fill = env
        .state
        .start_pipeline_cases_with_harness(
            "slow run".to_owned(),
            vec![
                json!({"label": "s0", "impl_prompt": "slow 0"}),
                json!({"label": "s1", "impl_prompt": "slow 1"}),
                json!({"label": "s2", "impl_prompt": "slow 2"}),
            ],
            harness_for("sleep 30"),
        )
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let lanes = env.store.list_lanes(&fill.id).await.unwrap();
        if lanes.len() == 3 && lanes.iter().all(|l| l.state == "running") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "filler lanes never started: {:?}",
            lanes.iter().map(|l| (&l.idx, &l.state)).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The subject: 3 lanes, 0 free slots → must be queued, not started.
    let subject = env
        .state
        .start_pipeline_cases_with_harness(
            "queued run".to_owned(),
            submitted_cases(),
            harness_for(""),
        )
        .await
        .unwrap();
    assert_eq!(subject.status, "queued", "lanes that don't fit must queue");
    assert!(env
        .store
        .list_queued_runs()
        .await
        .unwrap()
        .iter()
        .any(|r| r.id == subject.id));

    // ---- simulated restart: fresh AppState over the same DB ----
    let restarted = restart(&env, config, harness_for(""));
    let picked = restarted.pickup_queued_runs().await;
    assert_eq!(picked, 1, "exactly the stranded run is re-enqueued");

    // The pickup promotes and executes: judged, with the promotion audit trail.
    let status = wait_run_status(&env.store, &subject.id, &["judged", "failed"], 60).await;
    assert_eq!(
        status,
        "judged",
        "picked-up run must be promoted and judged; lanes: {:?}",
        env.store
            .list_lanes(&subject.id)
            .await
            .unwrap()
            .iter()
            .map(|l| (l.idx, l.state.clone(), l.error.clone()))
            .collect::<Vec<_>>()
    );
    let events = env.store.list_lane_events(&subject.id, 0).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.payload.contains("run promoted from queue")),
        "promotion must be logged: {:?}",
        events.iter().map(|e| &e.payload).collect::<Vec<_>>()
    );
    let lanes = env.store.list_lanes(&subject.id).await.unwrap();
    for lane in &lanes {
        assert_eq!(lane.state, "validated", "lane {} must validate", lane.idx);
    }
}
