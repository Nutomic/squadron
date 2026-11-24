//! F9: validator-phase failures retry IN PLACE — the implementer's sprite,
//! site, and workdir stay intact; only implementer-phase failures recreate
//! the sprite (run 7: a validator schema-gate hiccup escalated the harness
//! and recreated the sprite, burning a good implementer build).
#![allow(clippy::unwrap_used, clippy::panic)] // test code: panics on failure are the assertion mechanism

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use orchestrator::{
    HarnessMode, PipelineConfig, PipelineCtx, RunRequest, ScreenshotQueue, SlotSemaphore, SseHub,
};

mod common;

use common::wait_secs;
use sprites_client::mock_cli::MockState;
use sprites_client::{SpriteClient, SpriteConfig};
use store::Store;

/// Both tests in this file share process-global env vars (MOCK_SPRITE_STATE
/// et al.); they must not run concurrently or one deletes the other's state.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn canned_impl() -> String {
    serde_json::json!({
        "summary": "Implemented arcade page",
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
        "features": ["game loop"],
        "bugs": [],
        "security": "ok",
        "quality_score": 80.0,
        "summary": "Detailed validation: implementation matches claims and serves content."
    })
    .to_string()
}

async fn test_ctx(dir: &std::path::Path, retries: usize) -> Arc<PipelineCtx> {
    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    let sprite = SpriteClient::new(SpriteConfig {
        cli_path: option_env!("CARGO_BIN_EXE_mock-sprite-cli")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mock-sprite-cli")
            }),
        command_timeout: Duration::from_secs(10),
        ..SpriteConfig::default()
    });
    let config = PipelineConfig {
        n_lanes: 3,
        max_running_slots: 10,
        test_mode: true,
        data_dir: dir.join("data"),
        impl_deadline: Duration::from_secs(30),
        val_deadline: Duration::from_secs(30),
        judge_deadline: Duration::from_secs(15),
        retries_per_lane: retries,
        ..PipelineConfig::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let screenshots = ScreenshotQueue::with_data_dir(store.clone(), true, config.data_dir.clone());
    let slots = SlotSemaphore::new(config.max_running_slots);
    Arc::new(PipelineCtx {
        store: store.clone(),
        sprite,
        machine_llm: None,
        sse: SseHub::new(256),
        screenshots,
        config,
        slots,
        harness_cache: std::sync::Arc::new(orchestrator::harness::HarnessCache::new()),
        hub_registry: None,
        lane_queue: None,
    })
}

async fn wait_terminal(store: &Store, run_id: &str, secs: u64) -> store::Run {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let r = store.get_run(run_id).await.unwrap().unwrap();
        if r.status == "judged" || r.status == "failed" {
            return r;
        }
        if tokio::time::Instant::now() > deadline {
            let lanes = store.list_lanes(run_id).await.unwrap();
            panic!(
                "timeout waiting for run to finish: {:?}",
                lanes
                    .iter()
                    .map(|l| (&l.idx, &l.state, &l.error))
                    .collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn load_state(path: &std::path::Path) -> MockState {
    let json = std::fs::read_to_string(path).unwrap();
    serde_json::from_str(&json).unwrap()
}

fn lane_prefix(run_id: &str, n: usize) -> Vec<String> {
    let seg: String = run_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect::<String>()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .collect();
    (0..n).map(|i| format!("squadron-{seg}-impl-{i}")).collect()
}

fn create_count(state: &MockState, name: &str) -> usize {
    let wanted = format!("create {name}");
    state.command_log.iter().filter(|e| **e == wanted).count()
}

/// (c): a validator-phase failure retries the validator IN PLACE on the same
/// sprite: exactly one `create` per lane sprite, no second create, the
/// implementer session never reruns, and the lane still validates.
#[tokio::test(flavor = "multi_thread")]
async fn validator_failure_retries_in_place_without_sprite_recreation() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("mock-sprites-val-retry.json");
    std::env::set_var("MOCK_SPRITE_STATE", &state_path);
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let ctx = test_ctx(dir.path(), 1).await;
    let store = ctx.store.clone();

    // The validator command fails on its first invocation per lane (marker
    // file in the per-lane workdir) and succeeds on the second.
    let val = canned_val().replace('\'', "'\\''");
    let harness = HarnessMode::Command {
        impl_command: format!(
            "echo '<h1>hi</h1>' > index.html; echo '{}'",
            canned_impl().replace('\'', "'\\''")
        ),
        val_command: format!(
            "if [ -f val-retry-marker ]; then echo '{val}'; else touch val-retry-marker; echo 'validator transient blip'; exit 1; fi"
        ),
    };
    let run = orchestrator::start_run(
        ctx.clone(),
        RunRequest {
            brief: "Build a 90s arcade game".to_owned(),
            n_lanes: 3,
            harness,
        },
    )
    .await
    .unwrap();

    let final_run = wait_terminal(&store, &run.id, wait_secs()).await;
    assert_eq!(
        final_run.status,
        "judged",
        "the in-place validator retry must recover: lanes={:?}",
        store
            .list_lanes(&run.id)
            .await
            .unwrap()
            .iter()
            .map(|l| (&l.idx, &l.state, &l.error))
            .collect::<Vec<_>>()
    );

    let state = load_state(&state_path);
    for name in lane_prefix(&run.id, 3) {
        // F9 core assertion: NO second create for any lane.
        assert_eq!(
            create_count(&state, &name),
            1,
            "a validator-phase failure must never recreate the sprite ({name}); log: {:?}",
            state.command_log
        );
        // The implementer session ran exactly ONCE (no implementer rerun);
        // the validator session ran twice (first blipped, then succeeded).
        let sprite = state.sprites.get(&name).unwrap();
        assert_eq!(
            sprite.exec_count, 3,
            "impl(1) + validator(2) execs expected for {name}"
        );
    }

    // Every lane recovered to validated through the in-place retry.
    for lane in store.list_lanes(&run.id).await.unwrap() {
        assert_eq!(lane.state, "validated", "lane {}", lane.idx);
        let events = store.list_lane_events(&run.id, lane.idx).await.unwrap();
        assert!(
            events.iter().any(|e| e.kind == "log"
                && e.payload
                    .contains("retrying in place (sprite and site kept)")),
            "lane {} must log the in-place validator retry: {:?}",
            lane.idx,
            events.iter().map(|e| &e.payload).collect::<Vec<_>>()
        );
        assert_eq!(lane.val_verdict.as_deref(), Some("pass"));
    }

    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
    std::env::remove_var("MOCK_CREATE_MS");
    std::env::remove_var("MOCK_EXEC_MS");
}

/// (d): an IMPLEMENTER-phase failure still recreates the sprite: the lane
/// retry destroys and re-creates it (a second `create` in the mock log).
#[tokio::test(flavor = "multi_thread")]
async fn implementer_failure_still_recreates_the_sprite() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("mock-sprites-impl-retry.json");
    std::env::set_var("MOCK_SPRITE_STATE", &state_path);
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");
    std::env::set_var("MOCK_FAIL_FIRST_EXEC", "1");

    let ctx = test_ctx(dir.path(), 1).await;
    let store = ctx.store.clone();

    let harness = HarnessMode::Command {
        impl_command: format!(
            "echo '<h1>hi</h1>' > index.html; echo '{}'",
            canned_impl().replace('\'', "'\\''")
        ),
        val_command: format!("echo '{}'", canned_val().replace('\'', "'\\''")),
    };
    let run = orchestrator::start_run(
        ctx.clone(),
        RunRequest {
            brief: "Build a 90s arcade game".to_owned(),
            n_lanes: 2,
            harness,
        },
    )
    .await
    .unwrap();

    let final_run = wait_terminal(&store, &run.id, wait_secs()).await;
    assert_eq!(
        final_run.status,
        "judged",
        "the implementer retry must recover: lanes={:?}",
        store
            .list_lanes(&run.id)
            .await
            .unwrap()
            .iter()
            .map(|l| (&l.idx, &l.state, &l.error))
            .collect::<Vec<_>>()
    );

    let state = load_state(&state_path);
    // Exactly one exec failed (global hook), so exactly one lane hit the
    // implementer-phase retry path: total creates = lanes + 1 recreations.
    let names = lane_prefix(&run.id, 2);
    let total_creates: usize = names.iter().map(|n| create_count(&state, n)).sum();
    assert_eq!(
        total_creates, 3,
        "exactly one implementer-phase failure must have recreated its sprite; log: {:?}",
        state.command_log
    );
    // The recreated lane's destroy precedes its second create.
    assert!(
        state.command_log.iter().any(|e| e.starts_with("destroy ")),
        "the lane retry must destroy the sprite before recreating: {:?}",
        state.command_log
    );

    for lane in store.list_lanes(&run.id).await.unwrap() {
        assert_eq!(lane.state, "validated", "lane {}", lane.idx);
    }

    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
    std::env::remove_var("MOCK_CREATE_MS");
    std::env::remove_var("MOCK_EXEC_MS");
    std::env::remove_var("MOCK_FAIL_FIRST_EXEC");
}
