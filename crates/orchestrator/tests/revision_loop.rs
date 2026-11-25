#![allow(clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use orchestrator::{
    start_run_cases, HarnessMode, PipelineConfig, PipelineCtx, ScreenshotQueue, SlotSemaphore,
    SseHub,
};
use sprites_client::mock_cli::MockState;
use sprites_client::{SpriteClient, SpriteConfig};
use store::Store;

mod common;

use common::wait_secs;

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

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

fn impl_command() -> String {
    format!(
        "printf '%s\\n' pass >> impl-history; printf '%s' {}",
        shell_quote(&canned_impl())
    )
}

fn report(
    verdict: &str,
    bugs: &[&str],
    security: &[&str],
    quality_score: f64,
    summary: &str,
) -> String {
    serde_json::json!({
        "verdict": verdict,
        "features": [],
        "bugs": bugs,
        "security": security,
        "quality_score": quality_score,
        "summary": summary
    })
    .to_string()
}

fn validator_command(reports: &[String]) -> String {
    let mut cases = String::new();
    for (i, report) in reports.iter().enumerate() {
        cases.push_str(&format!(
            "{} ) printf '%s' {} ;; ",
            i + 1,
            shell_quote(report)
        ));
    }
    let fallback = reports
        .last()
        .map(|value| shell_quote(value))
        .unwrap_or_else(|| "''".to_owned());
    format!(
        "n=$(cat val-count 2>/dev/null || echo 0); n=$((n+1)); printf '%s\\n' \"$n\" > val-count; case \"$n\" in {cases}*) printf '%s' {fallback} ;; esac"
    )
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
    let config = PipelineConfig {
        n_lanes: 1,
        max_running_slots: 2,
        test_mode: true,
        data_dir: dir.join("data"),
        impl_deadline: Duration::from_secs(30),
        val_deadline: Duration::from_secs(30),
        judge_deadline: Duration::from_secs(15),
        retries_per_lane: 0,
        ..PipelineConfig::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let screenshots = ScreenshotQueue::with_data_dir(store.clone(), true, config.data_dir.clone());
    let ctx = Arc::new(PipelineCtx {
        store: store.clone(),
        sprite,
        machine_llm: None,
        sse: SseHub::new(256),
        screenshots,
        config,
        slots: SlotSemaphore::new(2),
        harness_cache: Arc::new(orchestrator::harness::HarnessCache::new()),
        hub_registry: None,
        lane_queue: None,
        create_throttle: Arc::new(orchestrator::SpriteCreateThrottle::disabled()),
    });
    (ctx, store)
}

fn set_mock_env(state_path: &std::path::Path) {
    std::env::set_var("MOCK_SPRITE_STATE", state_path);
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");
    std::env::set_var("MOCK_ORG_LIMIT_FAILS", "0");
    std::env::set_var("MOCK_RATE_LIMIT_FAILS", "0");
    std::env::remove_var("MOCK_FAIL_FIRST_EXEC");
}

fn clear_mock_env() {
    for key in [
        "MOCK_SPRITE_STATE",
        "MOCK_COLD_START_MS",
        "MOCK_CREATE_MS",
        "MOCK_EXEC_MS",
        "MOCK_ORG_LIMIT_FAILS",
        "MOCK_RATE_LIMIT_FAILS",
    ] {
        std::env::remove_var(key);
    }
}

async fn start_case(ctx: &Arc<PipelineCtx>, harness: HarnessMode) -> store::Run {
    let cases = vec![serde_json::json!({
        "label": "arcade",
        "impl_prompt": "Build a 90s arcade game landing page"
    })];
    start_run_cases(ctx.clone(), "Build a 90s arcade game", cases, harness, None)
        .await
        .unwrap()
}

async fn wait_terminal(store: &Store, run_id: &str) -> store::Run {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs());
    loop {
        let run = store.get_run(run_id).await.unwrap().unwrap();
        if run.status == "judged" || run.status == "partial" || run.status == "failed" {
            return run;
        }
        assert!(tokio::time::Instant::now() < deadline, "run never finished");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn event_count(events: &[store::LaneEvent], kind: &str) -> usize {
    events.iter().filter(|event| event.kind == kind).count()
}

fn impl_passes(dir: &std::path::Path, run_id: &str) -> usize {
    let path = dir
        .join("data/work")
        .join(run_id)
        .join("lane-0/impl-history");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .count()
}

fn assert_create_count(state_path: &std::path::Path, expected: usize) {
    let state: MockState =
        serde_json::from_str(&std::fs::read_to_string(state_path).unwrap_or_default())
            .unwrap_or_default();
    let creates = state
        .command_log
        .iter()
        .filter(|entry| entry.starts_with("create "))
        .count();
    assert_eq!(
        creates, expected,
        "sprite recreated unexpectedly: {:?}",
        state.command_log
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fail_then_pass_runs_one_revision_on_the_same_sprite() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("mock-revision-success.json");
    set_mock_env(&state_path);
    let (ctx, store) = test_ctx(dir.path()).await;

    let harness = HarnessMode::Command {
        impl_command: impl_command(),
        val_command: validator_command(&[
            report(
                "fail",
                &["score resets on reload"],
                &[],
                42.0,
                "Found a scoring defect",
            ),
            report("pass", &[], &[], 88.0, "Score now persists after reload"),
        ]),
    };
    let run = start_case(&ctx, harness).await;
    let final_run = wait_terminal(&store, &run.id).await;
    assert_eq!(
        final_run.status,
        "judged",
        "lanes: {:?}",
        store
            .list_lanes(&run.id)
            .await
            .unwrap()
            .iter()
            .map(|lane| (&lane.idx, &lane.state, &lane.error))
            .collect::<Vec<_>>()
    );

    let lane = store.get_lane(&run.id, 0).await.unwrap().unwrap();
    assert_eq!(lane.state, "validated");
    assert_eq!(lane.revisions, 1);
    assert_eq!(lane.val_verdict.as_deref(), Some("pass"));
    let events = store.list_lane_events(&run.id, 0).await.unwrap();
    assert_eq!(event_count(&events, "revision_prompt_sent"), 1);
    assert_eq!(event_count(&events, "revision_done"), 1);
    assert!(events
        .iter()
        .any(|event| event.kind == "state" && event.payload == "revising"));
    assert!(events
        .iter()
        .any(|event| event.kind == "revision_done" && event.payload == r#"{"revision":1}"#));
    assert_eq!(impl_passes(dir.path(), &run.id), 2);
    assert_create_count(&state_path, 1);
    clear_mock_env();
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_findings_stop_after_one_revision() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("mock-revision-no-progress.json");
    set_mock_env(&state_path);
    let (ctx, store) = test_ctx(dir.path()).await;

    let fails = report(
        "fail",
        &["score resets on reload"],
        &[],
        30.0,
        "Score still resets",
    );
    let harness = HarnessMode::Command {
        impl_command: impl_command(),
        val_command: validator_command(&[fails.clone(), fails]),
    };
    let run = start_case(&ctx, harness).await;
    let final_run = wait_terminal(&store, &run.id).await;
    assert_eq!(final_run.status, "failed");

    let lane = store.get_lane(&run.id, 0).await.unwrap().unwrap();
    assert_eq!(lane.state, "failed");
    assert_eq!(lane.revisions, 1);
    assert!(lane
        .error
        .as_deref()
        .is_some_and(|error| error.contains("no progress")));
    let events = store.list_lane_events(&run.id, 0).await.unwrap();
    assert_eq!(event_count(&events, "revision_prompt_sent"), 1);
    assert_eq!(event_count(&events, "revision_done"), 1);
    assert_eq!(impl_passes(dir.path(), &run.id), 2);
    assert_create_count(&state_path, 1);
    clear_mock_env();
}

#[tokio::test(flavor = "multi_thread")]
async fn distinct_findings_stop_at_two_revisions() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("mock-revision-cap.json");
    set_mock_env(&state_path);
    let (ctx, store) = test_ctx(dir.path()).await;

    let harness = HarnessMode::Command {
        impl_command: impl_command(),
        val_command: validator_command(&[
            report(
                "fail",
                &["score resets on reload"],
                &[],
                40.0,
                "First defect",
            ),
            report("fail", &["dead faq link"], &[], 35.0, "Second defect"),
            report("fail", &["missing favicon"], &[], 30.0, "Third defect"),
        ]),
    };
    let run = start_case(&ctx, harness).await;
    let final_run = wait_terminal(&store, &run.id).await;
    assert_eq!(final_run.status, "failed");

    let lane = store.get_lane(&run.id, 0).await.unwrap().unwrap();
    assert_eq!(lane.state, "failed");
    assert_eq!(lane.revisions, 2);
    assert!(lane
        .error
        .as_deref()
        .is_some_and(|error| error.contains("after 2 revisions")));
    let events = store.list_lane_events(&run.id, 0).await.unwrap();
    assert_eq!(event_count(&events, "revision_prompt_sent"), 2);
    assert_eq!(event_count(&events, "revision_done"), 2);
    assert!(events
        .iter()
        .any(|event| event.kind == "revision_done" && event.payload == r#"{"revision":2}"#));
    assert_eq!(impl_passes(dir.path(), &run.id), 3);
    assert_create_count(&state_path, 1);
    clear_mock_env();
}

#[tokio::test(flavor = "multi_thread")]
async fn fail_without_affirmative_findings_does_not_revise() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("mock-revision-unverifiable.json");
    set_mock_env(&state_path);
    let (ctx, store) = test_ctx(dir.path()).await;

    let harness = HarnessMode::Command {
        impl_command: impl_command(),
        val_command: validator_command(&[report(
            "fail",
            &[],
            &[],
            45.0,
            "cannot verify from evidence",
        )]),
    };
    let run = start_case(&ctx, harness).await;
    let final_run = wait_terminal(&store, &run.id).await;
    assert_eq!(final_run.status, "judged");

    let lane = store.get_lane(&run.id, 0).await.unwrap().unwrap();
    assert_eq!(lane.state, "validated");
    assert_eq!(lane.revisions, 0);
    assert_eq!(lane.val_verdict.as_deref(), Some("pass"));
    assert_eq!(lane.val_quality, Some(45.0));
    let events = store.list_lane_events(&run.id, 0).await.unwrap();
    assert_eq!(event_count(&events, "revision_prompt_sent"), 0);
    assert_eq!(event_count(&events, "revision_done"), 0);
    assert!(events
        .iter()
        .any(|event| event.kind == "log" && event.payload.contains("unverifiable pass")));
    assert_eq!(impl_passes(dir.path(), &run.id), 1);
    assert_create_count(&state_path, 1);
    clear_mock_env();
}
