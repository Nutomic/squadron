#![allow(clippy::unwrap_used, clippy::panic)] // test code: panics on failure are the assertion mechanism

//! Wave D integration tests: the global lane-slot queue (FIFO promotion),
//! slot-exhaustion spawn requeue, and worker progress probes — all against
//! the fake (mock CLI) sprite pipeline.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use orchestrator::{
    queue, HarnessMode, HubRegistry, LaneQueue, PipelineConfig, PipelineCtx, RunRequest,
    ScreenshotQueue, SlotSemaphore, SseHub,
};
use sprites_client::{SpriteClient, SpriteConfig};
use store::Store;

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
        "features": ["game loop", "score"],
        "bugs": [],
        "security": "no issues found",
        "quality_score": 88.5,
        "summary": "Detailed validation: implementation matches claims and serves content."
    })
    .to_string()
}

struct TestEnv {
    ctx: Arc<PipelineCtx>,
    store: Store,
    _tmp: tempfile::TempDir,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

fn harness_for(impl_body: &str) -> HarnessMode {
    let prefix = if impl_body.is_empty() {
        String::new()
    } else {
        format!("{impl_body}; ")
    };
    HarnessMode::Command {
        impl_command: format!(
            "{prefix}echo '<h1>hi</h1>' > index.html; echo '{}'",
            canned_impl().replace('\'', "'\\''")
        ),
        val_command: format!("echo '{}'", canned_val().replace('\'', "'\\''")),
    }
}

async fn setup(config: PipelineConfig) -> TestEnv {
    let _env_guard = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("mock-sprites.json");
    std::env::set_var("MOCK_SPRITE_STATE", &state_path);
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

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

    std::fs::create_dir_all(&config.data_dir).unwrap();
    let screenshots = ScreenshotQueue::with_data_dir(store.clone(), true, config.data_dir.clone());
    let slots = SlotSemaphore::new(config.max_running_slots);
    let hub = SseHub::new(256);
    let hub_registry: HubRegistry = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
    let queue = Arc::new(LaneQueue::new());
    queue::spawn_dispatcher(queue.clone());
    let ctx = Arc::new(PipelineCtx {
        store: store.clone(),
        sprite,
        machine_llm: None, // fallback judge
        sse: hub.clone(),
        screenshots,
        config,
        slots,
        harness_cache: Arc::new(orchestrator::harness::HarnessCache::new()),
        hub_registry: Some(hub_registry.clone()),
        lane_queue: Some(queue.clone()),
    });
    TestEnv {
        ctx,
        store,
        _tmp: dir,
        _guard: _env_guard,
    }
}

fn base_config(dir: &std::path::Path) -> PipelineConfig {
    PipelineConfig {
        n_lanes: 3,
        test_mode: true,
        data_dir: dir.join("data"),
        impl_deadline: Duration::from_secs(30),
        val_deadline: Duration::from_secs(30),
        judge_deadline: Duration::from_secs(15),
        ..PipelineConfig::default()
    }
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

async fn clear_env() {
    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
    std::env::remove_var("MOCK_CREATE_MS");
    std::env::remove_var("MOCK_EXEC_MS");
    std::env::remove_var("MOCK_ORG_LIMIT_FAILS");
}

/// Wave D: two runs totalling more than 8 lanes — the second run enters
/// `queued` status and is FIFO-promoted once the first run's lanes free slots.
#[tokio::test(flavor = "multi_thread")]
async fn two_runs_over_pool_second_queued_then_promoted() {
    let dir = tempfile::tempdir().unwrap();
    let env = setup(base_config(dir.path())).await;
    let TestEnv { ctx, store, .. } = &env;

    // Run A: 8 slow lanes hold the whole 8-slot pool.
    let req_a = RunRequest {
        brief: "slow run".to_owned(),
        n_lanes: 8,
        harness: harness_for("sleep 1"),
    };
    let run_a = orchestrator::start_run(ctx.clone(), req_a).await.unwrap();
    assert_eq!(run_a.status, "running");

    // wait until every lane of A is running (holding slots)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let lanes = store.list_lanes(&run_a.id).await.unwrap();
        if lanes.len() == 8 && lanes.iter().all(|l| l.state == "running") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "lanes never started: {:?}",
            lanes.iter().map(|l| (&l.idx, &l.state)).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Run B: 3 more lanes cannot fit (0 free slots) -> queued in the DB.
    let req_b = RunRequest {
        brief: "queued run".to_owned(),
        n_lanes: 3,
        harness: harness_for(""),
    };
    let run_b = orchestrator::start_run(ctx.clone(), req_b).await.unwrap();
    assert_eq!(
        run_b.status, "queued",
        "B must be queued while A holds the pool"
    );
    let queued = store.list_queued_runs().await.unwrap();
    assert!(queued.iter().any(|r| r.id == run_b.id));

    // A's lanes finish (and later B is promoted); both runs must be judged.
    wait_run_status(&env.store, &run_a.id, &["judged", "failed"], 60).await;
    wait_run_status(&env.store, &run_b.id, &["judged", "failed"], 60).await;
    let final_b = env.store.get_run(&run_b.id).await.unwrap().unwrap();
    assert_eq!(
        final_b.status, "judged",
        "queued run B must be promoted and judged"
    );
    // the promotion left an audit trail on lane 0
    let events = env.store.list_lane_events(&run_b.id, 0).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.payload.contains("run promoted from queue")),
        "promotion must be logged: {:?}",
        events.iter().map(|e| &e.payload).collect::<Vec<_>>()
    );
    clear_env().await;
}

/// Wave D: a spawn failure classified as slot exhaustion (org-limit error
/// shape) requeues the lane with backoff — the run must NOT fail.
#[tokio::test(flavor = "multi_thread")]
async fn slot_exhaustion_spawn_failure_requeues_lane_not_failed() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = base_config(dir.path());
    config.slot_requeue_backoff = Duration::from_millis(1);
    let env = setup(config).await;
    std::env::set_var("MOCK_ORG_LIMIT_FAILS", "2");

    let req = RunRequest {
        brief: "org-limit run".to_owned(),
        n_lanes: 3,
        harness: harness_for(""),
    };
    let run = orchestrator::start_run(env.ctx.clone(), req).await.unwrap();
    let status = wait_run_status(&env.store, &run.id, &["judged", "failed"], 60).await;
    assert_eq!(status, "judged", "slot exhaustion must not fail the run");

    let lanes = env.store.list_lanes(&run.id).await.unwrap();
    for lane in &lanes {
        assert_eq!(lane.state, "validated", "lane {} must validate", lane.idx);
        assert!(lane.error.is_none(), "lane error: {:?}", lane.error);
    }
    let mut saw_requeue = false;
    for lane in &lanes {
        let events = env.store.list_lane_events(&run.id, lane.idx).await.unwrap();
        if events
            .iter()
            .any(|e| e.payload.contains("slot exhaustion on spawn; requeueing"))
        {
            saw_requeue = true;
        }
    }
    assert!(saw_requeue, "a lane must have been requeued with backoff");
    clear_env().await;
}

/// Wave D probes: a parseable probe reply is stored as a `progress` lane
/// event and exposed on the lanes API JSON (progressPct + stage).
#[tokio::test(flavor = "multi_thread")]
async fn probe_happy_path_writes_event_and_api_field() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = base_config(dir.path());
    config.probe_interval = Duration::from_millis(30);
    config.probe_command =
        Some("printf '%s' '{\"progress_pct\":37,\"stage\":\"building ui\"}'".to_owned());
    let env = setup(config).await;

    let req = RunRequest {
        brief: "probe run".to_owned(),
        n_lanes: 3,
        harness: harness_for("sleep 0.4"),
    };
    let run = orchestrator::start_run(env.ctx.clone(), req).await.unwrap();
    wait_run_status(&env.store, &run.id, &["judged", "failed"], 60).await;

    let lanes = env.store.list_lanes(&run.id).await.unwrap();
    for lane in &lanes {
        let events = env.store.list_lane_events(&run.id, lane.idx).await.unwrap();
        let progress: Vec<_> = events.iter().filter(|e| e.kind == "progress").collect();
        assert!(
            !progress.is_empty(),
            "lane {} must record progress probes",
            lane.idx
        );
        assert!(
            progress[0].payload.contains("\"progress_pct\":37"),
            "payload: {}",
            progress[0].payload
        );
    }
    // the lanes API JSON exposes the latest probe report
    let snap = orchestrator::snapshot::build_snapshot(&env.store, &run.id)
        .await
        .unwrap();
    let json = serde_json::to_string(&snap).unwrap();
    assert!(json.contains("\"progressPct\":37"), "json: {json}");
    assert!(json.contains("\"stage\":\"building ui\""), "json: {json}");
    clear_env().await;
}

/// Wave D probes: an unparseable probe reply records nothing and the run
/// still completes (probes are best-effort telemetry).
#[tokio::test(flavor = "multi_thread")]
async fn unparseable_probe_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = base_config(dir.path());
    config.probe_interval = Duration::from_millis(30);
    config.probe_command = Some("printf '%s' 'i am not json at all'".to_owned());
    let env = setup(config).await;

    let req = RunRequest {
        brief: "garbage probe run".to_owned(),
        n_lanes: 3,
        harness: harness_for("sleep 0.4"),
    };
    let run = orchestrator::start_run(env.ctx.clone(), req).await.unwrap();
    wait_run_status(&env.store, &run.id, &["judged", "failed"], 60).await;

    let lanes = env.store.list_lanes(&run.id).await.unwrap();
    for lane in &lanes {
        let events = env.store.list_lane_events(&run.id, lane.idx).await.unwrap();
        assert!(
            events.iter().all(|e| e.kind != "progress"),
            "unparseable probe output must be ignored (lane {})",
            lane.idx
        );
    }
    let snap = orchestrator::snapshot::build_snapshot(&env.store, &run.id)
        .await
        .unwrap();
    assert!(snap.lanes.iter().all(|l| l.progress_pct.is_none()));
    clear_env().await;
}

/// Wave D: the FIFO dispatcher must never skip a head that does not fit: a
/// 9-lane run (larger than the 8-slot pool) queued behind a full pool stays
/// queued until the pool is completely free.
#[tokio::test(flavor = "multi_thread")]
async fn oversized_run_waits_for_fully_free_pool() {
    let dir = tempfile::tempdir().unwrap();
    let env = setup(base_config(dir.path())).await;
    let TestEnv { ctx, store, .. } = &env;

    // Fill 3 of 8 slots with a slow 3-lane run.
    let req_a = RunRequest {
        brief: "small run".to_owned(),
        n_lanes: 3,
        harness: harness_for("sleep 1"),
    };
    let run_a = orchestrator::start_run(ctx.clone(), req_a).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let lanes = store.list_lanes(&run_a.id).await.unwrap();
        if lanes.len() == 3 && lanes.iter().all(|l| l.state == "running") {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // 9 lanes > 8 slots: must queue even though 5 slots are free.
    let req_b = RunRequest {
        brief: "oversized run".to_owned(),
        n_lanes: 9,
        harness: harness_for(""),
    };
    let run_b = orchestrator::start_run(ctx.clone(), req_b).await.unwrap();
    assert_eq!(
        run_b.status, "queued",
        "oversized run queues unless the pool is fully free"
    );

    wait_run_status(&env.store, &run_a.id, &["judged", "failed"], 60).await;
    wait_run_status(&env.store, &run_b.id, &["judged", "failed"], 60).await;
    let final_b = env.store.get_run(&run_b.id).await.unwrap().unwrap();
    assert_eq!(final_b.status, "judged");
    clear_env().await;
}
