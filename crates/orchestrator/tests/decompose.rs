#![allow(clippy::unwrap_used, clippy::panic)] // test code: panics on failure are the assertion mechanism

//! Wave B fake-LLM pipeline tests: a local stub plays the machine LLM for the
//! brief-decomposition and judge calls, the lanes run through the test-only
//! Command harness, and the assertions check what the orchestrator actually
//! persisted: lane count derived from the decomposition output, the 8-case
//! hard cap + truncation event, empty-output retries, and the raw-brief
//! fallback.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// Serialize tests that mutate the shared mock-CLI env vars.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use axum::extract::{Json as AxumJson, State};
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};

mod common;

use common::wait_secs;
use llm_gateway::MachineLlmClient;
use orchestrator::{
    HarnessMode, PipelineConfig, PipelineCtx, RunRequest, ScreenshotQueue, SlotSemaphore, SseHub,
};
use serde_json::{json, Value};
use sprites_client::{SpriteClient, SpriteConfig};
use store::Store;

fn canned_impl() -> String {
    serde_json::json!({
        "summary": "Implemented the case page",
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
        "features": ["the case page"],
        "bugs": [],
        "security": "ok",
        "quality_score": 80.0,
        "summary": "Detailed validation: implementation matches claims and serves content."
    })
    .to_string()
}

/// The fake model: simulates a decomposition that follows the prompt rules.
/// Explicit number in the brief wins; " or " alternatives multiply (2^n for n
/// independent groups); otherwise the 6-case default.
fn fake_model_case_count(brief: &str) -> usize {
    if let Some(digits) = brief
        .split_whitespace()
        .find_map(|word| {
            word.chars()
                .all(|c| c.is_ascii_digit())
                .then(|| word.to_owned())
        })
        .and_then(|word| word.parse::<usize>().ok())
    {
        return digits;
    }
    let or_groups = brief.matches(" or ").count();
    if or_groups > 0 {
        return 2usize.saturating_pow(or_groups as u32);
    }
    6
}

fn cases_json(count: usize, brief: &str) -> String {
    let cases: Vec<Value> = (0..count)
        .map(|i| {
            json!({
                "label": format!("case-{i}-of-{count}"),
                "impl_prompt": format!("Full implementer prompt for case {i}: {brief}. Serve on port 8080, self-verify the public URL, no external CDNs, 1h deadline."),
            })
        })
        .collect();
    json!({ "cases": cases }).to_string()
}

/// Judge reply sized from the number of `lane_idx` keys in the judge prompt.
fn judge_json_for(body: &Value) -> Value {
    let body_text = body.to_string();
    let lane_count = body_text.matches("lane_idx").count().max(1);
    let ranking: Vec<usize> = (0..lane_count).collect();
    json!({
        "ranking": ranking,
        "winner": 0,
        "verdict_card": "Case 0 wins. Well built.",
        "reasons": ["stub judge picked the first lane"],
        "case_fulfillment": {"state": "unverifiable", "evidence": "stub judge has no real evidence"}
    })
}

#[derive(Default)]
struct StubState {
    decompose_calls: AtomicUsize,
    bodies: Mutex<Vec<Value>>,
    sessions: Mutex<Vec<String>>,
}

fn stub_router(state: Arc<StubState>, empty_first: usize) -> Router {
    Router::new()
        .route(
            "/chat/completions",
            post(
                move |State(st): State<Arc<StubState>>,
                      headers: HeaderMap,
                      AxumJson(body): AxumJson<Value>| async move {
                    st.bodies.lock().unwrap().push(body.clone());
                    if let Some(session) = headers.get("x-opencode-session") {
                        st.sessions
                            .lock()
                            .unwrap()
                            .push(session.to_str().unwrap_or_default().to_owned());
                    }
                    let system = body["messages"]
                        .as_array()
                        .and_then(|msgs| msgs.first())
                        .and_then(|m| m["content"].as_str())
                        .unwrap_or_default();
                    // The decomposition call identifies itself by its role
                    // preamble; everything else here is the judge call.
                    if system.contains("brief decomposer") {
                        let n = st.decompose_calls.fetch_add(1, Ordering::SeqCst) + 1;
                        let content = if n <= empty_first {
                            String::new()
                        } else {
                            let brief = body["messages"]
                                .as_array()
                                .and_then(|msgs| msgs.get(1))
                                .and_then(|m| m["content"].as_str())
                                .and_then(|user| {
                                    user.split("<data label=\"brief\">").nth(1)
                                })
                                .and_then(|rest| rest.split("</data>").next())
                                .unwrap_or_default()
                                .trim()
                                .to_owned();
                            cases_json(fake_model_case_count(&brief), &brief)
                        };
                        return (
                            axum::http::StatusCode::OK,
                            Json(json!({
                                "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
                                "usage": {"opencode": {"allowance": {"usd": 0.01}}}
                            })),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        Json(json!({
                            "choices": [{"index": 0, "message": {"role": "assistant", "content": judge_json_for(&body).to_string()}, "finish_reason": "stop"}],
                            "usage": {"opencode": {"allowance": {"usd": 0.02}}}
                        })),
                    )
                },
            ),
        )
        .with_state(state)
}

async fn spawn_stub(empty_first: usize) -> (String, Arc<StubState>) {
    let state = Arc::new(StubState::default());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let served = state.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, stub_router(served, empty_first)).await;
    });
    (format!("http://{addr}/chat/completions"), state)
}

async fn test_ctx(
    dir: &std::path::Path,
    machine_llm: Option<Arc<MachineLlmClient>>,
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
    let config = PipelineConfig {
        n_lanes: 3,
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
        machine_llm,
        sse: SseHub::new(256),
        screenshots,
        config,
        slots: SlotSemaphore::new(10),
        harness_cache: Arc::new(orchestrator::harness::HarnessCache::new()),
        hub_registry: None,
        lane_queue: None,
        create_throttle: std::sync::Arc::new(orchestrator::SpriteCreateThrottle::disabled()),
    });
    (ctx, store)
}

async fn wait_run(store: &Store, run_id: &str, secs: u64) -> store::Run {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let r = store.get_run(run_id).await.unwrap().unwrap();
        if r.status == "judged" || r.status == "partial" || r.status == "failed" {
            return r;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("timeout waiting for run to finish");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn command_harness() -> HarnessMode {
    HarnessMode::Command {
        impl_command: format!("echo '{}'", canned_impl().replace('\'', "'\\''")),
        val_command: format!("echo '{}'", canned_val().replace('\'', "'\\''")),
    }
}

async fn start(ctx: Arc<PipelineCtx>, brief: &str) -> store::Run {
    orchestrator::start_run(
        ctx.clone(),
        RunRequest {
            brief: brief.to_owned(),
            n_lanes: 3, // deprecated: decomposition output must decide
            harness: command_harness(),
        },
    )
    .await
    .unwrap()
}

fn wire_checks(state: &StubState, run_id: &str) {
    let bodies = state.bodies.lock().unwrap();
    assert!(!bodies.is_empty(), "no machine calls captured");
    for (i, body) in bodies.iter().enumerate() {
        assert_eq!(
            body["reasoning_effort"],
            json!("low"),
            "request #{i} must send reasoning_effort=low: {body}"
        );
    }
    let sessions = state.sessions.lock().unwrap();
    assert!(
        sessions
            .first()
            .map(|s| s.starts_with("squadron-decompose-"))
            .unwrap_or(false),
        "first machine call must use the squadron-decompose session id: {sessions:?}"
    );
    assert!(
        sessions
            .first()
            .map(|s| s.contains(&format!("decompose-{run_id}")))
            .unwrap_or(false),
        "decompose session must embed the run id: {sessions:?}"
    );
}

/// "green or red product website in framework A or B" -> 2 x 2 = 4 cases = 4
/// lanes, each carrying a distinct full implementer prompt.
#[tokio::test(flavor = "multi_thread")]
async fn or_alternatives_multiply_into_four_lanes() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let (endpoint, stub) = spawn_stub(0).await;
    let llm = MachineLlmClient::new("test-key-not-a-secret", &endpoint)
        .with_timeout(Duration::from_secs(10));
    let (ctx, store) = test_ctx(dir.path(), Some(Arc::new(llm))).await;

    let brief = "green or red product website in framework A or B";
    let run = start(ctx, brief).await;
    let final_run = wait_run(&store, &run.id, wait_secs()).await;
    assert_eq!(final_run.status, "judged", "4 lanes must judge");

    let lanes = store.list_lanes(&run.id).await.unwrap();
    assert_eq!(lanes.len(), 4, "2x2 or-alternatives must give 4 lanes");
    assert_eq!(final_run.n_lanes, 4, "run row must reflect decomposition");
    let mut prompts = std::collections::HashSet::new();
    for lane in &lanes {
        let variant: Value = serde_json::from_str(&lane.variant_json).unwrap();
        assert!(variant["impl_prompt"].as_str().unwrap().contains(brief));
        prompts.insert(variant["impl_prompt"].as_str().unwrap().to_owned());
    }
    assert_eq!(prompts.len(), 4, "case prompts must be distinct");
    assert_eq!(stub.decompose_calls.load(Ordering::SeqCst), 1);
    wire_checks(&stub, &run.id);
}

/// "5 different colors" -> exactly 5 cases = 5 lanes.
#[tokio::test(flavor = "multi_thread")]
async fn explicit_number_brief_creates_five_lanes() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let (endpoint, stub) = spawn_stub(0).await;
    let llm = MachineLlmClient::new("test-key-not-a-secret", &endpoint)
        .with_timeout(Duration::from_secs(10));
    let (ctx, store) = test_ctx(dir.path(), Some(Arc::new(llm))).await;

    let run = start(ctx, "poster wall with 5 different colors").await;
    let final_run = wait_run(&store, &run.id, wait_secs()).await;
    assert_eq!(final_run.status, "judged");

    let lanes = store.list_lanes(&run.id).await.unwrap();
    assert_eq!(lanes.len(), 5, "explicit 5 must give 5 lanes");
    assert_eq!(final_run.n_lanes, 5);
    wire_checks(&stub, &run.id);
}

/// A plain brief (no "or", no number) -> the 6-case default = 6 lanes.
#[tokio::test(flavor = "multi_thread")]
async fn plain_brief_defaults_to_six_lanes() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let (endpoint, stub) = spawn_stub(0).await;
    let llm = MachineLlmClient::new("test-key-not-a-secret", &endpoint)
        .with_timeout(Duration::from_secs(10));
    let (ctx, store) = test_ctx(dir.path(), Some(Arc::new(llm))).await;

    let run = start(ctx, "a tidy landing page for a bakery").await;
    let final_run = wait_run(&store, &run.id, wait_secs()).await;
    assert_eq!(final_run.status, "judged");

    let lanes = store.list_lanes(&run.id).await.unwrap();
    assert_eq!(lanes.len(), 6, "plain brief must default to 6 lanes");
    assert_eq!(final_run.n_lanes, 6);
    wire_checks(&stub, &run.id);
}

/// A model that returns 12 cases is capped at 8 lanes in Rust, and the
/// truncation is logged as a lane event.
#[tokio::test(flavor = "multi_thread")]
async fn twelve_cases_are_capped_at_eight_with_truncation_event() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let (endpoint, stub) = spawn_stub(0).await;
    let llm = MachineLlmClient::new("test-key-not-a-secret", &endpoint)
        .with_timeout(Duration::from_secs(10));
    let (ctx, store) = test_ctx(dir.path(), Some(Arc::new(llm))).await;

    let run = start(ctx, "12 variants of a dashboard").await;
    let final_run = wait_run(&store, &run.id, wait_secs()).await;
    assert_eq!(final_run.status, "judged");

    let lanes = store.list_lanes(&run.id).await.unwrap();
    assert_eq!(lanes.len(), 8, ">8 cases must be truncated to 8 lanes");
    assert_eq!(final_run.n_lanes, 8);
    let events = store.list_lane_events(&run.id, 0).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.payload.contains("truncated from 12 to 8")),
        "truncation must be logged as a lane event: {:?}",
        events.iter().map(|e| &e.payload).collect::<Vec<_>>()
    );
    wire_checks(&stub, &run.id);
}

/// The known low-effort flake: the first attempt answers empty, the retry
/// succeeds — the run proceeds with 6 lanes and the retry is logged.
#[tokio::test(flavor = "multi_thread")]
async fn empty_output_is_retried_then_succeeds() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let (endpoint, stub) = spawn_stub(1).await;
    let llm = MachineLlmClient::new("test-key-not-a-secret", &endpoint)
        .with_timeout(Duration::from_secs(10));
    let (ctx, store) = test_ctx(dir.path(), Some(Arc::new(llm))).await;

    let run = start(ctx, "a tidy landing page for a bakery").await;
    let final_run = wait_run(&store, &run.id, wait_secs()).await;
    assert_eq!(final_run.status, "judged");

    assert_eq!(
        stub.decompose_calls.load(Ordering::SeqCst),
        2,
        "one retry after the empty first attempt"
    );
    let lanes = store.list_lanes(&run.id).await.unwrap();
    assert_eq!(lanes.len(), 6);
    let events = store.list_lane_events(&run.id, 0).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.payload.contains("decompose attempt 1 rejected: empty")),
        "the empty attempt must be logged: {:?}",
        events.iter().map(|e| &e.payload).collect::<Vec<_>>()
    );
}

/// Four empty outputs exhaust the retry budget: the run falls back to ONE
/// lane carrying the raw brief instead of hard-failing the submission.
#[tokio::test(flavor = "multi_thread")]
async fn four_empty_outputs_fall_back_to_single_raw_brief_lane() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let (endpoint, stub) = spawn_stub(usize::MAX).await;
    let llm = MachineLlmClient::new("test-key-not-a-secret", &endpoint)
        .with_timeout(Duration::from_secs(10));
    let (ctx, store) = test_ctx(dir.path(), Some(Arc::new(llm))).await;

    let brief = "a tidy landing page for a bakery";
    let run = start(ctx, brief).await;
    let final_run = wait_run(&store, &run.id, wait_secs()).await;
    // F18: a single surviving lane still reaches the judge (judge ranks the
    // survivors; only an empty validated set fails the run), so the fallback
    // lane is judged — but it must exist and be driven by the brief.
    assert_eq!(final_run.status, "judged");

    assert_eq!(
        stub.decompose_calls.load(Ordering::SeqCst),
        4,
        "initial call + 3 retries, then the fallback"
    );
    let lanes = store.list_lanes(&run.id).await.unwrap();
    assert_eq!(lanes.len(), 1, "fallback is a single raw-brief lane");
    let variant: Value = serde_json::from_str(&lanes[0].variant_json).unwrap();
    assert_eq!(variant["impl_prompt"].as_str().unwrap(), brief);
    let events = store.list_lane_events(&run.id, 0).await.unwrap();
    assert!(
        events.iter().any(|e| e
            .payload
            .contains("falling back to a single raw-brief case")),
        "the fallback must be logged: {:?}",
        events.iter().map(|e| &e.payload).collect::<Vec<_>>()
    );
}
