#![allow(clippy::unwrap_used, clippy::panic)] // test code: panics on failure are the assertion mechanism

//! F29 fake-gateway regressions: the judge call must never kill a run. A local
//! stub replays the production HTTP 400 for the primary image-capable judge,
//! then controls how far the ladder gets before a verdict is produced.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use axum::extract::{Json as AxumJson, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use llm_gateway::{MachineLlmClient, JUDGE_IMAGE_FALLBACK_MODEL, JUDGE_MODEL};
use orchestrator::{
    HarnessMode, PipelineConfig, PipelineCtx, RunRequest, ScreenshotQueue, SlotSemaphore, SseHub,
};
use serde_json::{json, Value};
use sprites_client::{SpriteClient, SpriteConfig};
use store::Store;

mod common;

use common::wait_secs;

#[derive(Clone, Copy)]
enum Failure {
    PrimaryImages,
    BothImageRungs,
    AllRungs,
}

#[derive(Default)]
struct StubState {
    bodies: std::sync::Mutex<Vec<Value>>,
}

fn cases_json() -> String {
    json!({"cases": [
        {
            "label": "case-0",
            "impl_prompt": "Build case 0: a small page on port 8080, self-verify the public URL."
        },
        {
            "label": "case-1",
            "impl_prompt": "Build case 1: a small page on port 8080, self-verify the public URL."
        }
    ]})
    .to_string()
}

fn judge_json_for(body: &Value) -> Value {
    let lane_count = body.to_string().matches("lane_idx").count().max(1);
    let ranking: Vec<usize> = (0..lane_count).collect();
    json!({
        "ranking": ranking,
        "winner": 0,
        "verdict_card": "Case 0 wins. Well built.",
        "reasons": ["stub judge picked the first lane"],
        "brief_fulfillment": {"state": "unverifiable", "evidence": "stub judge has no real evidence"}
    })
}

fn router(state: Arc<StubState>, failure: Failure) -> Router {
    Router::new()
        .route(
            "/chat/completions",
            post(move |State(st): State<Arc<StubState>>, AxumJson(body): AxumJson<Value>| async move {
                st.bodies.lock().unwrap().push(body.clone());
                let system = body["messages"]
                    .as_array()
                    .and_then(|msgs| msgs.first())
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or_default();
                if system.contains("brief decomposer") {
                    return (
                        StatusCode::OK,
                        Json(json!({
                            "choices": [{"index": 0, "message": {"role": "assistant", "content": cases_json()}, "finish_reason": "stop"}],
                            "usage": {"opencode": {"allowance": {"usd": 0.01}}}
                        })),
                    );
                }
                let model = body["model"].as_str().unwrap_or_default();
                let has_images = body["messages"][1]["content"].is_array();
                let reject = match failure {
                    Failure::PrimaryImages => model == JUDGE_MODEL && has_images,
                    Failure::BothImageRungs => has_images,
                    Failure::AllRungs => true,
                };
                if reject {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": {"type": "invalid_request"},
                            "message": "Upstream request failed: [invalid_request] the request is invalid"
                        })),
                    );
                }
                (
                    StatusCode::OK,
                    Json(json!({
                        "choices": [{"index": 0, "message": {"role": "assistant", "content": judge_json_for(&body).to_string()}, "finish_reason": "stop"}],
                        "usage": {"opencode": {"allowance": {"usd": 0.02}}}
                    })),
                )
            }),
        )
        .with_state(state)
}

async fn spawn_stub(failure: Failure) -> (String, Arc<StubState>) {
    let state = Arc::new(StubState::default());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let served = state.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(served, failure)).await;
    });
    (format!("http://{addr}/chat/completions"), state)
}

fn judge_calls(state: &StubState) -> Vec<Value> {
    state
        .bodies
        .lock()
        .unwrap()
        .iter()
        .filter(|body| {
            !body["messages"][0]["content"]
                .as_str()
                .unwrap_or_default()
                .contains("brief decomposer")
        })
        .cloned()
        .collect()
}

fn image_urls(call: &Value) -> Vec<Value> {
    call["messages"][1]["content"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p["type"] == "image_url")
                .map(|p| p["image_url"]["url"].clone())
                .collect()
        })
        .unwrap_or_default()
}

fn canned_impl() -> String {
    json!({
        "summary": "Implemented the case page",
        "files": 3,
        "run_command": "python3 -m http.server 8080",
        "done": true,
        "blockers": []
    })
    .to_string()
}

fn canned_val() -> String {
    json!({
        "verdict": "pass",
        "features": ["the case page"],
        "bugs": [],
        "security": "ok",
        "quality_score": 80.0,
        "summary": "Detailed validation: implementation matches claims and serves content."
    })
    .to_string()
}

fn blocked_impl() -> String {
    json!({
        "summary": "Blocked",
        "files": 0,
        "done": false,
        "blockers": ["rate limited on spawn"]
    })
    .to_string()
}

fn per_lane_command(fail_lane: i64, fail_json: &str, pass_json: &str) -> String {
    format!(
        "case \"$PWD\" in *lane-{fail_lane}*) echo '{}' ;; *) echo '{}' ;; esac",
        fail_json.replace('\'', "'\\''"),
        pass_json.replace('\'', "'\\''")
    )
}

async fn test_ctx(dir: &std::path::Path, endpoint: &str) -> (Arc<PipelineCtx>, Store) {
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
        n_lanes: 2,
        max_running_slots: 10,
        retries_per_lane: 0,
        test_mode: true,
        data_dir: dir.join("data"),
        impl_deadline: Duration::from_secs(30),
        val_deadline: Duration::from_secs(30),
        judge_deadline: Duration::from_secs(15),
        ..PipelineConfig::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let screenshots = ScreenshotQueue::with_data_dir(store.clone(), true, config.data_dir.clone());
    let ctx = Arc::new(PipelineCtx {
        store: store.clone(),
        sprite,
        machine_llm: Some(Arc::new(MachineLlmClient::new("test-key", endpoint))),
        sse: SseHub::new(256),
        screenshots,
        config,
        slots: SlotSemaphore::new(10),
        harness_cache: Arc::new(orchestrator::harness::HarnessCache::new()),
        hub_registry: None,
        lane_queue: None,
        create_throttle: Arc::new(orchestrator::SpriteCreateThrottle::disabled()),
    });
    (ctx, store)
}

async fn start(ctx: Arc<PipelineCtx>, fail_lane: Option<i64>) -> store::Run {
    let impl_command = match fail_lane {
        Some(idx) => per_lane_command(idx, &blocked_impl(), &canned_impl()),
        None => format!("echo '{}'", canned_impl().replace('\'', "'\\''")),
    };
    orchestrator::start_run(
        ctx,
        RunRequest {
            brief: "Build 2 case pages".to_owned(),
            n_lanes: 2,
            harness: HarnessMode::Command {
                impl_command,
                val_command: format!("echo '{}'", canned_val().replace('\'', "'\\''")),
            },
        },
    )
    .await
    .unwrap()
}

async fn wait_run(store: &Store, run_id: &str) -> store::Run {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs());
    loop {
        let run = store.get_run(run_id).await.unwrap().unwrap();
        if run.status == "judged" || run.status == "partial" || run.status == "failed" {
            return run;
        }
        assert!(tokio::time::Instant::now() < deadline, "run did not finish");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn env_setup(dir: &std::path::Path) {
    std::env::set_var("MOCK_SPRITE_STATE", dir.join("mock-state.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");
}

async fn assert_rung(store: &Store, run: &store::Run, label: &str) {
    let expected = format!("judge_rung: {label}");
    for lane in store.list_lanes(&run.id).await.unwrap() {
        let events = store.list_lane_events(&run.id, lane.idx).await.unwrap();
        assert!(
            events.iter().any(|e| e.payload == expected),
            "lane {} missing {expected}: {:?}",
            lane.idx,
            events.iter().map(|e| &e.payload).collect::<Vec<_>>()
        );
    }
}

async fn assert_no_run_failed(store: &Store, run: &store::Run) {
    for lane in store.list_lanes(&run.id).await.unwrap() {
        let events = store.list_lane_events(&run.id, lane.idx).await.unwrap();
        assert!(
            !events.iter().any(|e| e.payload.starts_with("run_failed:")),
            "lane {} logged run_failed: {:?}",
            lane.idx,
            events.iter().map(|e| &e.payload).collect::<Vec<_>>()
        );
    }
}

/// (a) the production 400 on the primary image rung must promote the flash
/// variant with the identical screenshot parts, not fail the run.
#[tokio::test(flavor = "multi_thread")]
async fn primary_judge_400_uses_flash_with_images() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    env_setup(dir.path());
    let (endpoint, stub) = spawn_stub(Failure::PrimaryImages).await;
    let (ctx, store) = test_ctx(dir.path(), &endpoint).await;
    let run = start(ctx, None).await;
    let final_run = wait_run(&store, &run.id).await;

    assert_eq!(final_run.status, "judged");
    assert!(final_run.winner_lane.is_some());
    let calls = judge_calls(&stub);
    assert_eq!(calls.len(), 2, "primary then flash: {calls:?}");
    assert_eq!(calls[0]["model"], json!(JUDGE_MODEL));
    assert_eq!(calls[1]["model"], json!(JUDGE_IMAGE_FALLBACK_MODEL));
    assert!(calls[0]["messages"][1]["content"].is_array());
    assert_eq!(
        image_urls(&calls[0]),
        image_urls(&calls[1]),
        "flash rung must reuse the same screenshot parts"
    );
    assert_rung(&store, &final_run, "glm-5.3-flash+images").await;
    assert_no_run_failed(&store, &final_run).await;
}

/// (b) when both image rungs reject the request, the big model is retried
/// text-only before the deterministic fallback is considered.
#[tokio::test(flavor = "multi_thread")]
async fn both_image_rungs_failing_falls_back_to_text_only() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    env_setup(dir.path());
    let (endpoint, stub) = spawn_stub(Failure::BothImageRungs).await;
    let (ctx, store) = test_ctx(dir.path(), &endpoint).await;
    let run = start(ctx, None).await;
    let final_run = wait_run(&store, &run.id).await;

    assert_eq!(final_run.status, "judged");
    let calls = judge_calls(&stub);
    assert_eq!(calls.len(), 3, "all three rungs tried: {calls:?}");
    assert_eq!(calls[0]["model"], json!(JUDGE_MODEL));
    assert_eq!(calls[1]["model"], json!(JUDGE_IMAGE_FALLBACK_MODEL));
    assert_eq!(calls[2]["model"], json!(JUDGE_MODEL));
    let text = calls[2]["messages"][1]["content"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("validator-reports"),
        "text rung keeps the prompt"
    );
    assert_rung(&store, &final_run, "glm-5.3 text-only").await;
    assert_no_run_failed(&store, &final_run).await;
}

/// (c) exact production shape: one lane validates while the other fails, and
/// every judge rung 400s. The run must still be `partial` with a schema-valid
/// deterministic winner among survivors.
#[tokio::test(flavor = "multi_thread")]
async fn all_judge_rungs_failing_still_yields_partial_with_fallback_winner() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    env_setup(dir.path());
    let (endpoint, stub) = spawn_stub(Failure::AllRungs).await;
    let (ctx, store) = test_ctx(dir.path(), &endpoint).await;
    let run = start(ctx, Some(1)).await;
    let final_run = wait_run(&store, &run.id).await;

    assert_eq!(final_run.status, "partial");
    assert_eq!(final_run.winner_lane, Some(0), "winner among survivors");
    assert_eq!(
        final_run.judge_verdict.as_deref(),
        Some("Winner selected by fallback. Criteria met.")
    );
    let lanes = store.list_lanes(&run.id).await.unwrap();
    assert_eq!(
        lanes.iter().find(|l| l.idx == 0).unwrap().state,
        "validated"
    );
    assert_eq!(lanes.iter().find(|l| l.idx == 1).unwrap().state, "failed");
    assert_eq!(judge_calls(&stub).len(), 3, "every rung was attempted");
    assert_rung(&store, &final_run, "deterministic_fallback").await;
}

/// (d) a completely dead judge ladder never reaches the run_failed handler.
#[tokio::test(flavor = "multi_thread")]
async fn judge_errors_never_reach_the_run_failed_path() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    env_setup(dir.path());
    let (endpoint, _stub) = spawn_stub(Failure::AllRungs).await;
    let (ctx, store) = test_ctx(dir.path(), &endpoint).await;
    let run = start(ctx, None).await;
    let final_run = wait_run(&store, &run.id).await;

    assert_ne!(
        final_run.status, "failed",
        "judge errors are not run errors"
    );
    assert_eq!(final_run.status, "judged");
    assert!(final_run.judge_verdict.is_some());
    assert_no_run_failed(&store, &final_run).await;
    assert_rung(&store, &final_run, "deterministic_fallback").await;
}
