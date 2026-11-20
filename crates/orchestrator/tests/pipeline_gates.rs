use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

// Serialize tests that mutate the shared mock-CLI env vars.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use orchestrator::{
    HarnessMode, PipelineConfig, PipelineCtx, RunRequest, ScreenshotQueue, SlotSemaphore, SseHub,
};
use sprites_client::{SpriteClient, SpriteConfig};
use store::Store;

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

async fn test_ctx(
    dir: &std::path::Path,
    slots: usize,
    impl_deadline: Duration,
) -> (Arc<PipelineCtx>, Store) {
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
    let mut config = PipelineConfig::default();
    config.n_lanes = 3;
    config.max_running_slots = slots;
    config.retries_per_lane = 0;
    config.test_mode = true;
    config.data_dir = dir.join("data");
    std::fs::create_dir_all(&config.data_dir).unwrap();
    config.impl_deadline = impl_deadline;
    config.val_deadline = Duration::from_secs(30);
    config.judge_deadline = Duration::from_secs(15);
    let screenshots = ScreenshotQueue::new(store.clone(), true);
    let ctx = Arc::new(PipelineCtx {
        store: store.clone(),
        sprite,
        machine_llm: None,
        sse: SseHub::new(256),
        screenshots,
        config,
        slots: SlotSemaphore::new(slots),
        harness_cache: Arc::new(orchestrator::harness::HarnessCache::new()),
        hub_registry: None,
    });
    (ctx, store)
}

async fn wait_run(store: &Store, run_id: &str, secs: u64) -> store::Run {
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

/// #22: an implementer that exceeds impl_deadline fails the lane; the error
/// carries the deadline class and no implementer summary is persisted.
#[tokio::test(flavor = "multi_thread")]
async fn impl_deadline_fails_lane_with_class_error() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let (ctx, store) = test_ctx(dir.path(), 10, Duration::from_millis(300)).await;

    let harness = HarnessMode::Command {
        impl_command: format!("sleep 5; echo '{}'", canned_impl().replace('\'', "'\\''")),
        val_command: format!("echo '{}'", canned_val().replace('\'', "'\\''")),
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

    let final_run = wait_run(&store, &run.id, 30).await;
    assert_eq!(
        final_run.status, "failed",
        "no lane can finish past deadline"
    );

    for lane in store.list_lanes(&run.id).await.unwrap() {
        assert_eq!(lane.state, "failed", "lane {:?}", lane.idx);
        assert!(lane.impl_summary.is_none(), "no impl summary on deadline");
        let err = lane.error.unwrap_or_default();
        assert!(
            err.contains("deadline") || err.contains("implementer"),
            "error must carry the failure class, got: {err}"
        );
    }
}

/// #23a: implementer output that fails the schema gate fails the lane instead
/// of advancing.
#[tokio::test(flavor = "multi_thread")]
async fn schema_gate_rejection_fails_lane() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let (ctx, store) = test_ctx(dir.path(), 10, Duration::from_secs(30)).await;

    let harness = HarnessMode::Command {
        impl_command: "echo 'this is not json at all'".to_owned(),
        val_command: format!("echo '{}'", canned_val().replace('\'', "'\\''")),
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

    let final_run = wait_run(&store, &run.id, 30).await;
    assert_eq!(final_run.status, "failed");

    for lane in store.list_lanes(&run.id).await.unwrap() {
        assert_eq!(lane.state, "failed", "lane {:?}", lane.idx);
        assert!(lane.impl_summary.is_none());
    }
}

/// #23b: more lanes than slots must queue, not deadlock; all lanes finish.
#[tokio::test(flavor = "multi_thread")]
async fn slot_exhaustion_queues_lanes_and_completes() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let (ctx, store) = test_ctx(dir.path(), 1, Duration::from_secs(30)).await;
    assert_eq!(ctx.slots.available(), 1);

    let harness = HarnessMode::Command {
        impl_command: format!("echo '{}'", canned_impl().replace('\'', "'\\''")),
        val_command: format!("echo '{}'", canned_val().replace('\'', "'\\''")),
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

    let final_run = wait_run(&store, &run.id, 60).await;
    assert_eq!(final_run.status, "judged");
    for lane in store.list_lanes(&run.id).await.unwrap() {
        assert_eq!(lane.state, "validated", "lane {:?}", lane.idx);
    }
    assert_eq!(ctx.slots.available(), 1, "permits released after the run");
}

/// #24: fewer than two validated lanes ends the run as failed, never judged.
#[tokio::test(flavor = "multi_thread")]
async fn run_fails_when_fewer_than_two_lanes_validate() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let (ctx, store) = test_ctx(dir.path(), 10, Duration::from_secs(30)).await;

    // Implementer claims not done -> lane failure per #13, so zero validated.
    let harness = HarnessMode::Command {
        impl_command: format!(
            "echo '{}'",
            serde_json::json!({
                "summary": "Blocked",
                "files": 0,
                "done": false,
                "blockers": ["missing dependency"]
            })
            .to_string()
            .replace('\'', "'\\''")
        ),
        val_command: format!("echo '{}'", canned_val().replace('\'', "'\\''")),
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

    let final_run = wait_run(&store, &run.id, 30).await;
    assert_eq!(final_run.status, "failed");
    assert_eq!(final_run.winner_lane, None);
    for lane in store.list_lanes(&run.id).await.unwrap() {
        assert_eq!(lane.state, "failed", "lane {:?}", lane.idx);
    }
}
