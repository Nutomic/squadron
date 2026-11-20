#![allow(clippy::unwrap_used, clippy::panic)] // test code: panics on failure are the assertion mechanism

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use orchestrator::{
    HarnessMode, HubRegistry, PipelineConfig, PipelineCtx, RunRequest, ScreenshotQueue,
    SlotSemaphore, SseHub,
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
        "features": ["game loop", "score"],
        "bugs": [],
        "security": "no issues found",
        "quality_score": 88.5,
        "summary": "Detailed validation: implementation matches claims and serves content."
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn smoke_pipeline_mock_sprite_n3_judged_and_persisted() {
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

    let config = PipelineConfig {
        n_lanes: 3,
        max_running_slots: 10,
        test_mode: true,
        data_dir: dir.path().join("data"),
        impl_deadline: Duration::from_secs(30),
        val_deadline: Duration::from_secs(30),
        judge_deadline: Duration::from_secs(15),
        ..PipelineConfig::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();

    // Audit R3 finding 4: data dir comes from the config, never the /data
    // fallback (suite must pass on a non-root dev box).
    let screenshots = ScreenshotQueue::with_data_dir(store.clone(), true, config.data_dir.clone());
    let slots = SlotSemaphore::new(config.max_running_slots);
    let hub = SseHub::new(256);
    // Audit R3 finding 5: the registry must not leak a channel per run.
    let hub_registry: HubRegistry = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
    let ctx = Arc::new(PipelineCtx {
        store: store.clone(),
        sprite,
        machine_llm: None, // fallback judge
        sse: hub.clone(),
        screenshots,
        config,
        slots,
        harness_cache: std::sync::Arc::new(orchestrator::harness::HarnessCache::new()),
        hub_registry: Some(hub_registry.clone()),
    });

    let harness = HarnessMode::Command {
        impl_command: format!(
            "echo '<h1>hi</h1>' > index.html; echo '{}'",
            canned_impl().replace('\'', "'\\''")
        ),
        val_command: format!("echo '{}'", canned_val().replace('\'', "'\\''")),
    };
    let req = RunRequest {
        brief: "Build a 90s arcade game".to_owned(),
        n_lanes: 3,
        harness,
    };

    let run = orchestrator::start_run(ctx.clone(), req).await.unwrap();
    assert_eq!(run.n_lanes, 3);

    // wait for completion (background task)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let r = store.get_run(&run.id).await.unwrap().unwrap();
        if r.status == "judged" || r.status == "failed" {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            let lanes = store.list_lanes(&run.id).await.unwrap();
            panic!(
                "timeout waiting for run to finish, status={} lanes={:?}",
                r.status,
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
        final_run.status, "judged",
        "run should be judged: verdict={:?}",
        final_run.judge_verdict
    );
    assert!(
        final_run.judge_verdict.is_some(),
        "judge verdict card persisted"
    );
    assert!(final_run.winner_lane.is_some(), "winner selected");

    let lanes = store.list_lanes(&run.id).await.unwrap();
    assert_eq!(lanes.len(), 3);
    for lane in &lanes {
        assert_eq!(
            lane.state, "validated",
            "lane {} should be validated, got {} err={:?}",
            lane.idx, lane.state, lane.error
        );
        assert_eq!(lane.val_verdict.as_deref(), Some("pass"));
        assert!(lane.impl_summary.is_some());
        assert!(lane.tarball_path.is_some(), "artifacts tarball recorded");
        assert!(lane.tokei_json.is_some(), "tokei recorded");
        assert!(lane.console_tail.is_some(), "console tail recorded");
        // R2 finding 2: renders are now ensured before the judge phase, so a
        // completed (judged) run has terminal screenshot states, not pending.
        assert_eq!(
            lane.screenshot_state.as_deref(),
            Some("done"),
            "screenshot rendered before judging"
        );
        let events = store.list_lane_events(&run.id, lane.idx).await.unwrap();
        assert!(
            !events.is_empty(),
            "lane {} should have events recorded",
            lane.idx
        );
        assert!(
            events.iter().any(|e| e.kind == "state"),
            "state events recorded"
        );
        // #17: a `cost` lane event must be appended atomically with each cost
        // update, so the dashboard sparkline can always render.
        assert!(
            events.iter().any(|e| e.kind == "cost"),
            "cost events recorded for sparkline"
        );
        // #18: the full validator report (features/bugs/security) is persisted
        // and hydratable.
        assert!(
            lane.val_report_json.is_some(),
            "validator report persisted for lane {}",
            lane.idx
        );
        let report: prompts::ValidatorOutput =
            serde_json::from_str(lane.val_report_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(&report.features).unwrap(),
            serde_json::json!(["game loop", "score"]),
            "features hydrate from persisted report"
        );
    }

    // DB persisted and queryable
    let runs = store.list_runs().await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run.id);

    // Audit R3 finding 5: the terminal (judged) run must have removed its hub
    // from the registry — no leaked broadcast channel per run.
    assert!(
        hub_registry.read().await.is_empty(),
        "hub registry must not retain a terminal run's hub"
    );

    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
    std::env::remove_var("MOCK_CREATE_MS");
    std::env::remove_var("MOCK_EXEC_MS");
}
