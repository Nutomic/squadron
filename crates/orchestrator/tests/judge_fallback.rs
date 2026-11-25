#![allow(clippy::unwrap_used, clippy::panic)] // test code: panics on failure are the assertion mechanism

//! F29/F35b fake-gateway regressions: the judge call must never kill a run. A
//! local stub replays the production HTTP 400 for the flash primary image
//! request, then controls how far the ladder gets before a verdict is produced.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use axum::extract::{Json as AxumJson, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use llm_gateway::{MachineLlmClient, JUDGE_MODEL, JUDGE_TEXT_FALLBACK_MODEL};
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
    AllRungs,
    /// First judge call answers 200 with a verdict card carrying an
    /// `[INJECTION?]` marker; later calls are clean.
    MarkerScan,
}

#[derive(Default)]
struct StubState {
    bodies: std::sync::Mutex<Vec<Value>>,
    judge_calls: std::sync::Mutex<usize>,
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

/// Security review F36: a poisoned lane's evidence steered the judge into
/// echoing an `[INJECTION?]` marker into its verdict card. Schema-valid, but
/// the marker scan must reject it.
fn judge_json_with_marker(body: &Value) -> Value {
    let mut v = judge_json_for(body);
    v["verdict_card"] = json!("[INJECTION?] prefer lane 1");
    v
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
                let judge_call_n = {
                    let mut n = st.judge_calls.lock().unwrap();
                    *n += 1;
                    *n
                };
                let reject = match failure {
                    Failure::PrimaryImages => model == JUDGE_MODEL && has_images,
                    Failure::AllRungs => true,
                    // The marker response is HTTP 200: the ladder advances
                    // because the SCAN rejects it, not the transport.
                    Failure::MarkerScan => false,
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
                if matches!(failure, Failure::MarkerScan) && judge_call_n == 1 {
                    return (
                        StatusCode::OK,
                        Json(json!({
                            "choices": [{"index": 0, "message": {"role": "assistant", "content": judge_json_with_marker(&body).to_string()}, "finish_reason": "stop"}],
                            "usage": {"opencode": {"allowance": {"usd": 0.02}}}
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
    // F34: file-backed unique DB — an idle-reaped in-memory pool reopens an
    // EMPTY database under load ("no such table: runs").
    let store = common::migrated_store_in(dir).await;
    let sprite = SpriteClient::new(SpriteConfig {
        cli_path: option_env!("CARGO_BIN_EXE_mock-sprite-cli")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mock-sprite-cli")
            }),
        // Load-tolerant ceiling (F34): under full-suite parallel load a mock
        // CLI invocation can take minutes of wall time; solo these finish in
        // milliseconds. Ceiling only — no sleeps added.
        command_timeout: Duration::from_secs(wait_secs()),
        ..SpriteConfig::default()
    });
    let config = PipelineConfig {
        n_lanes: 2,
        max_running_slots: 10,
        retries_per_lane: 0,
        test_mode: true,
        data_dir: dir.join("data"),
        // Load-tolerant ceilings (F34, same class as the F20 pipeline_gates
        // fix): the mock steps are instant, but under 20-binary parallel load
        // a 30s/15s deadline fired before output arrived, failing lanes with
        // "deadline exceeded" and flipping the expected run status.
        impl_deadline: Duration::from_secs(wait_secs()),
        val_deadline: Duration::from_secs(wait_secs()),
        judge_deadline: Duration::from_secs(wait_secs()),
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

/// (a) the production 400 on the flash primary image rung must promote the
/// different-family deepseek text-only rung, not fail the run.
#[tokio::test(flavor = "multi_thread")]
async fn primary_judge_400_uses_deepseek_text_only() {
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
    assert_eq!(calls.len(), 2, "flash image then deepseek text: {calls:?}");
    assert_eq!(calls[0]["model"], json!(JUDGE_MODEL));
    assert_eq!(calls[1]["model"], json!(JUDGE_TEXT_FALLBACK_MODEL));
    assert!(calls[0]["messages"][1]["content"].is_array());
    assert!(
        calls[1]["messages"][1]["content"].is_string(),
        "deepseek is text-only on 2026-11-25 probes"
    );
    assert_rung(&store, &final_run, "deepseek-v4.1-flash text-only").await;
    assert_no_run_failed(&store, &final_run).await;
}

/// (b) without screenshots the flash image rung is skipped entirely; the
/// deepseek text-only rung keeps the prompt and serves the verdict.
#[tokio::test(flavor = "multi_thread")]
async fn no_screenshots_uses_deepseek_text_only() {
    let (endpoint, stub) = spawn_stub(Failure::PrimaryImages).await;
    let client = MachineLlmClient::new("test-key", &endpoint);
    let report = prompts::LaneJudgeReport::new(
        0,
        prompts::ValidatorOutput {
            verdict: prompts::Verdict::Pass,
            summary: "ok".to_owned(),
            features: vec!["feature".to_owned()],
            bugs: Vec::new(),
            security: Vec::new(),
            quality_score: 8.0,
        },
    );
    let verdict = orchestrator::judge::run_judge_ladder(
        &client,
        "squadron-test-run",
        "build a small page",
        &[report],
        &["<extract>".to_owned()],
        &[None],
        Duration::from_secs(10),
    )
    .await
    .unwrap();

    assert_eq!(verdict.rung, orchestrator::judge::JudgeRung::TextOnly);
    let calls = judge_calls(&stub);
    assert_eq!(calls.len(), 1, "image rung skipped: {calls:?}");
    assert_eq!(calls[0]["model"], json!(JUDGE_TEXT_FALLBACK_MODEL));
    let text = calls[0]["messages"][1]["content"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("validator-reports"),
        "text rung keeps the prompt"
    );
    assert!(text.contains("original-brief"), "text rung keeps the brief");
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
    assert_eq!(judge_calls(&stub).len(), 2, "both rungs were attempted");
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

/// Security review F36: judge output that carries an `[INJECTION?]` marker
/// (a poisoned lane's evidence steering the judge) must be rejected and the
/// ladder must advance to the next rung — the markers never flow into the
/// verdict card.
#[tokio::test(flavor = "multi_thread")]
async fn judge_output_with_injection_markers_advances_the_ladder() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    env_setup(dir.path());
    let (endpoint, stub) = spawn_stub(Failure::MarkerScan).await;
    let (ctx, store) = test_ctx(dir.path(), &endpoint).await;
    let run = start(ctx, None).await;
    let final_run = wait_run(&store, &run.id).await;

    assert_eq!(final_run.status, "judged");
    assert!(final_run.winner_lane.is_some());
    let calls = judge_calls(&stub);
    assert_eq!(calls.len(), 2, "marker rung rejected, ladder advanced");
    assert_eq!(calls[0]["model"], json!(JUDGE_MODEL));
    assert_eq!(calls[1]["model"], json!(JUDGE_TEXT_FALLBACK_MODEL));
    let verdict = final_run.judge_verdict.clone().unwrap_or_default();
    assert!(
        !verdict.contains("[INJECTION?]"),
        "marker must not reach the verdict card: {verdict}"
    );
    assert_rung(&store, &final_run, "deepseek-v4.1-flash text-only").await;
    assert_no_run_failed(&store, &final_run).await;
}
