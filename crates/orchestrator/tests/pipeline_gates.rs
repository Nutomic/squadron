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
        assert_eq!(lane.state, "judged", "lane {:?}", lane.idx);
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

fn canned_fail() -> String {
    serde_json::json!({
        "verdict": "fail",
        "features": [],
        "bugs": ["page does not render"],
        "security": "ok",
        "quality_score": 20.0,
        "summary": "Detailed validation: implementation does not meet the brief."
    })
    .to_string()
}

/// Per-lane validator output: the listed lanes report fail, the rest pass. The
/// mock exec runs the command with the lane workdir as cwd, so $PWD selects it.
fn per_lane_val_command(fail_lanes: &[i64], fail_json: &str, pass_json: &str) -> String {
    let fail = fail_json.replace('\'', "'\\''");
    let pass = pass_json.replace('\'', "'\\''");
    let patterns = fail_lanes
        .iter()
        .map(|i| format!("*lane-{i}*"))
        .collect::<Vec<_>>()
        .join("|");
    format!("case \"$PWD\" in {patterns}) echo '{fail}' ;; *) echo '{pass}' ;; esac")
}

/// #24 (round 2): a validator fail verdict excludes the lane from the judge
/// set: the run is judged over the passing lanes only, and the failing lane
/// never becomes the winner.
#[tokio::test(flavor = "multi_thread")]
async fn validator_fail_verdict_excludes_lane_from_judge_set() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let (ctx, store) = test_ctx(dir.path(), 10, Duration::from_secs(30)).await;

    let harness = HarnessMode::Command {
        impl_command: format!("echo '{}'", canned_impl().replace('\'', "'\\''")),
        val_command: per_lane_val_command(&[0], &canned_fail(), &canned_val()),
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
        final_run.status, "judged",
        "two passing lanes can be judged"
    );
    // The judge set is {1, 2}: the fallback judge crowns the first passing lane.
    assert_eq!(
        final_run.winner_lane,
        Some(1),
        "winner must be a passing lane"
    );

    let mut seen_fail_excluded = false;
    for lane in store.list_lanes(&run.id).await.unwrap() {
        if lane.idx == 0 {
            // Excluded from the judge set: never judged, never the winner.
            assert_eq!(lane.val_verdict.as_deref(), Some("fail"), "lane 0");
            assert_eq!(lane.state, "validated", "lane 0 not judged");
            seen_fail_excluded = true;
        } else {
            assert_eq!(
                lane.val_verdict.as_deref(),
                Some("pass"),
                "lane {}",
                lane.idx
            );
            assert_eq!(lane.state, "judged", "lane {}", lane.idx);
        }
    }
    assert!(seen_fail_excluded, "lane 0 must carry the fail verdict");
}

/// #24 (round 2): with fewer than two passing verdicts there is no judge set,
/// so the run fails even though every lane reached validation.
#[tokio::test(flavor = "multi_thread")]
async fn single_pass_verdict_fails_run_without_judging() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let (ctx, store) = test_ctx(dir.path(), 10, Duration::from_secs(30)).await;

    let harness = HarnessMode::Command {
        impl_command: format!("echo '{}'", canned_impl().replace('\'', "'\\''")),
        val_command: per_lane_val_command(&[1, 2], &canned_fail(), &canned_val()),
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
        "only one passing lane: no judge"
    );
    assert_eq!(final_run.winner_lane, None);
    for lane in store.list_lanes(&run.id).await.unwrap() {
        assert_eq!(lane.state, "validated", "lane {:?}", lane.idx);
        assert!(lane.val_verdict.is_some(), "verdict recorded");
    }
}
