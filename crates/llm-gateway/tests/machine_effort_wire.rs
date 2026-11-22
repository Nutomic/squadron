#![allow(clippy::unwrap_used)] // test code: panics on failure are the assertion mechanism

//! Wave A hard rule: ALL machine-side inference uses low reasoning effort.
//! This is a WIRE-LEVEL assertion: a local stub records every request body
//! the MachineLlmClient actually sends, across every call path — plain
//! `chat` (diversity), `chat_with_session` (judge, with image parts), a
//! request that tries to set a higher effort itself (the client must
//! override it), and the transient-error retry path. One passing path is not
//! enough; every path must carry `reasoning_effort: "low"`.
//!
//! The sprite-side opencode harness has the matching guarantee
//! (`opencode run --variant low`, tested in orchestrator), and the curl-floor
//! recipe serializes `reasoning_effort: "low"` from its default (also
//! asserted here via the curl command text).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Json as AxumJson, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use llm_gateway::{
    machine_session_id, ChatMessage, ChatRequest, ContentPart, MachineLlmClient, DIVERSITY_MODEL,
    JUDGE_MODEL,
};
use serde_json::{json, Value};

type Captured = Arc<Mutex<Vec<Value>>>;

fn stub_router(state: (Captured, Arc<AtomicUsize>), fail_first: bool) -> Router {
    let (captured, attempts) = state;
    Router::new()
        .route(
            "/chat/completions",
            post(
                move |State((captured, attempts)): State<(Captured, Arc<AtomicUsize>)>,
                      AxumJson(body): AxumJson<Value>| async move {
                    captured.lock().unwrap().push(body);
                    let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    if fail_first && n == 1 {
                        return (
                            StatusCode::TOO_MANY_REQUESTS,
                            Json(json!({"error": "busy"})),
                        );
                    }
                    (
                        StatusCode::OK,
                        Json(json!({
                            "choices": [{"index": 0, "message": {"role": "assistant", "content": "{\"ok\":true}"}, "finish_reason": "stop"}],
                            "usage": {"opencode": {"allowance": {"usd": 0.01}}}
                        })),
                    )
                },
            ),
        )
        .with_state((captured, attempts))
}

async fn spawn_stub(fail_first: bool) -> (String, Captured) {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let state = (captured.clone(), Arc::new(AtomicUsize::new(0)));
    tokio::spawn(async move {
        let _ = axum::serve(listener, stub_router((state.0, state.1), fail_first)).await;
    });
    (format!("http://{addr}/chat/completions"), captured)
}

fn assert_all_bodies_low(captured: &Captured, context: &str) {
    let bodies = captured.lock().unwrap();
    assert!(!bodies.is_empty(), "{context}: no requests captured");
    for (i, body) in bodies.iter().enumerate() {
        assert_eq!(
            body["reasoning_effort"],
            json!("low"),
            "{context}: request #{i} must send reasoning_effort=low, got: {body}"
        );
    }
}

/// Diversity-style: plain `chat`, no session.
#[tokio::test]
async fn diversity_path_sends_low_reasoning_effort() {
    let (endpoint, captured) = spawn_stub(false).await;
    let client = MachineLlmClient::new("test-key-not-a-secret", &endpoint);
    let request = ChatRequest::new(DIVERSITY_MODEL)
        .system("diversity system")
        .user("diversity user");
    client.chat(&request).await.unwrap();
    assert_all_bodies_low(&captured, "diversity chat");
}

/// Judge-style: `chat_with_session` with image parts, exactly as the judge
/// sends screenshots (orchestrator/src/judge.rs).
#[tokio::test]
async fn judge_path_sends_low_reasoning_effort() {
    let (endpoint, captured) = spawn_stub(false).await;
    let client = MachineLlmClient::new("test-key-not-a-secret", &endpoint);
    let request = ChatRequest::new(JUDGE_MODEL)
        .system("judge system")
        .message(ChatMessage::user_parts(vec![
            ContentPart::text("rank lanes"),
            ContentPart::image("data:image/png;base64,AAAA"),
        ]))
        .temperature(0.2);
    client
        .chat_with_session(&request, &machine_session_id("run-1"))
        .await
        .unwrap();
    assert_all_bodies_low(&captured, "judge chat_with_session");
}

/// A call site that forgets the effort knob (or sets it explicitly high) must
/// still go out with low: the client enforces its configured effort on every
/// outbound request, so one forgotten builder cannot raise it.
#[tokio::test]
async fn client_overrides_request_level_effort_back_to_low() {
    let (endpoint, captured) = spawn_stub(false).await;
    let client = MachineLlmClient::new("test-key-not-a-secret", &endpoint);
    let request = ChatRequest::new("any-model")
        .user("hi")
        .reasoning_effort("high");
    assert_eq!(
        request.reasoning_effort.as_deref(),
        Some("high"),
        "precondition: the request itself asks for high"
    );
    client.chat(&request).await.unwrap();
    assert_all_bodies_low(&captured, "effort-override request");
}

/// The transient-error retry path re-sends the request: the retry must carry
/// low effort too (a 429 is answered with a second, identical-effort call).
#[tokio::test]
async fn retry_attempts_send_low_reasoning_effort() {
    let (endpoint, captured) = spawn_stub(true).await;
    let client = MachineLlmClient::new("test-key-not-a-secret", &endpoint);
    client
        .chat(&ChatRequest::new(DIVERSITY_MODEL).user("hi"))
        .await
        .unwrap();
    {
        let bodies = captured.lock().unwrap();
        assert_eq!(
            bodies.len(),
            2,
            "429 must be retried once (transient), got {} attempts",
            bodies.len()
        );
    }
    assert_all_bodies_low(&captured, "retry path");
}

/// The curl-floor harness path: the recipe serializes its requests (and the
/// shell curl command the sprite runs) with the recipe's low effort default.
#[test]
fn curl_floor_recipe_body_sends_low_reasoning_effort() {
    use llm_gateway::SpriteRecipe;
    let recipe = SpriteRecipe::for_model("run-1-0", "glm-5.3-flash");
    let request = recipe.build("system", "user");
    assert_eq!(
        request.reasoning_effort.as_deref(),
        Some("low"),
        "recipe default must be low"
    );
    let curl = recipe.curl_command(&request).unwrap();
    assert!(
        curl.contains(r#"--data '{\"reasoning_effort\":\"low\""#)
            || curl
                .replace('\\', "")
                .contains("\"reasoning_effort\":\"low\""),
        "curl command must embed reasoning_effort=low in the JSON body: {curl}"
    );
}
