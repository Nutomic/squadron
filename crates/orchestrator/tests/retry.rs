use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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

async fn test_ctx(dir: &std::path::Path) -> (Arc<PipelineCtx>, Store) {
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
    config.max_running_slots = 10;
    config.test_mode = true;
    config.data_dir = dir.join("data");
    std::fs::create_dir_all(&config.data_dir).unwrap();
    config.impl_deadline = Duration::from_secs(30);
    config.val_deadline = Duration::from_secs(30);
    config.judge_deadline = Duration::from_secs(15);
    let screenshots = ScreenshotQueue::new(store.clone(), true);
    let slots = SlotSemaphore::new(config.max_running_slots);
    let ctx = Arc::new(PipelineCtx {
        store: store.clone(),
        sprite,
        machine_llm: None,
        sse: SseHub::new(256),
        screenshots,
        config,
        slots,
        harness_cache: std::sync::Arc::new(orchestrator::harness::HarnessCache::new()),
        hub_registry: None,
    });
    (ctx, store)
}

/// #8: the retry path destroys the previous sprite, resets state, and can
/// succeed on the second attempt against the mock's fail-first-exec hook.
#[tokio::test(flavor = "multi_thread")]
async fn lane_retry_destroys_sprite_and_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("mock-sprites-retry.json");
    std::env::set_var("MOCK_SPRITE_STATE", &state_path);
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");
    std::env::set_var("MOCK_FAIL_FIRST_EXEC", "1");

    let (ctx, store) = test_ctx(dir.path()).await;

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
            n_lanes: 3,
            harness,
        },
    )
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let r = store.get_run(&run.id).await.unwrap().unwrap();
        if r.status == "judged" || r.status == "failed" {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            let lanes = store.list_lanes(&run.id).await.unwrap();
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

    let final_run = store.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(
        final_run.status,
        "judged",
        "retry must recover from the first transient exec failure; lanes={:?}",
        store
            .list_lanes(&run.id)
            .await
            .unwrap()
            .iter()
            .map(|l| (&l.idx, &l.state, &l.error))
            .collect::<Vec<_>>()
    );

    // At least one lane hit the transient failure, logged the attempt, and
    // recovered; every lane ends judged (validated -> judged after the winner).
    let mut saw_retry = false;
    for lane in store.list_lanes(&run.id).await.unwrap() {
        assert_eq!(lane.state, "judged", "lane {} recovered", lane.idx);
        let events = store.list_lane_events(&run.id, lane.idx).await.unwrap();
        if events
            .iter()
            .any(|e| e.kind == "log" && e.payload.contains("attempt 1 failed"))
        {
            saw_retry = true;
        }
    }
    assert!(
        saw_retry,
        "at least one lane should have logged attempt 1 failing"
    );

    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
    std::env::remove_var("MOCK_CREATE_MS");
    std::env::remove_var("MOCK_EXEC_MS");
    std::env::remove_var("MOCK_FAIL_FIRST_EXEC");
}
