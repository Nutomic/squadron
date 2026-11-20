use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Json;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::Router;
use llm_gateway::sprites_gateway::{GATEWAY_ENDPOINT, SESSION_HEADER};
use llm_gateway::{
    lane_model, parse_allowance, ChatMessage, ChatRequest, ContentPart, LlmError, MachineLlmClient,
    MessageContent, SpriteRecipe, DIVERSITY_MODEL, JUDGE_MODEL, USER_AGENT,
};
use serde_json::{json, Value};

#[derive(Clone, Default)]
struct Capture {
    headers: Arc<Mutex<Vec<(String, String)>>>,
    bodies: Arc<Mutex<Vec<Value>>>,
    attempts: Arc<AtomicUsize>,
}

impl Capture {
    fn record(&self, headers: &HeaderMap, body: &Value) {
        let mut h = self.headers.lock().unwrap();
        h.clear();
        for (name, value) in headers {
            h.push((
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            ));
        }
        self.bodies.lock().unwrap().push(body.clone());
        self.attempts.fetch_add(1, Ordering::SeqCst);
    }

    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .lock()
            .unwrap()
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }

    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

fn response(content: &str, allowance: Value) -> Value {
    json!({
        "id": "chatcmpl-test",
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7,
                   "opencode": { "allowance": allowance } }
    })
}

async fn serve(router: Router) -> u16 {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    port
}

#[tokio::test]
async fn machine_client_sends_key_and_user_agent_and_parses_cost() {
    let capture = Capture::default();
    let seen = capture.clone();
    let router = Router::new().route(
        "/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let seen = seen.clone();
            async move {
                seen.record(&headers, &body);
                Json(response("{\"ok\":true}", json!(0.0456)))
            }
        }),
    );
    let port = serve(router).await;
    let client = MachineLlmClient::new(
        "test-key-not-a-secret",
        format!("http://127.0.0.1:{port}/chat/completions"),
    );
    let request = ChatRequest::new(DIVERSITY_MODEL)
        .system("system prompt")
        .user("user prompt");
    let result = client.chat(&request).await.unwrap();
    assert_eq!(result.text().as_deref(), Some("{\"ok\":true}"));
    assert_eq!(result.cost_usd(), Some(0.0456));
    assert_eq!(
        capture.header("authorization").as_deref(),
        Some("Bearer test-key-not-a-secret")
    );
    assert_eq!(capture.header("user-agent").as_deref(), Some(USER_AGENT));
    let body = &capture.bodies.lock().unwrap()[0];
    assert_eq!(body["model"], json!(DIVERSITY_MODEL));
    assert_eq!(body["messages"][0]["role"], json!("system"));
    assert_eq!(body["messages"][1]["content"], json!("user prompt"));
    assert_eq!(body["reasoning_effort"], json!("low"));
    assert_eq!(capture.attempts(), 1);
}

#[tokio::test]
async fn sprite_recipe_uses_gateway_headers_without_auth() {
    assert_eq!(
        GATEWAY_ENDPOINT,
        "https://api.sprites.dev/v1/gateway/custom_api/sanCSEiLn9Np-G_Y5q2jKA/chat/completions"
    );
    assert_eq!(JUDGE_MODEL, "glm-5.3");
    assert_eq!(lane_model(0), "glm-5.3-flash");
    assert_eq!(lane_model(1), "deepseek-v4.1-flash");
    let recipe = SpriteRecipe::for_model("lane-7", lane_model(0));
    assert_eq!(recipe.session_id(), "swarm-lane-7");
    let headers = recipe.headers();
    assert!(headers
        .iter()
        .any(|(k, v)| k == "user-agent" && v == USER_AGENT));
    assert!(headers
        .iter()
        .any(|(k, v)| k == SESSION_HEADER && v == "swarm-lane-7"));
    assert!(!headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("authorization")));

    let recipe = recipe.with_endpoint("https://example.invalid/chat/completions");
    let curl = recipe
        .curl_command(&recipe.build("system", "user"))
        .unwrap();
    assert!(curl.contains(GATEWAY_ENDPOINT) || curl.contains("example.invalid"));
    assert!(curl.contains(SESSION_HEADER));
    assert!(!curl.to_ascii_lowercase().contains("authorization"));
}

#[tokio::test]
async fn transient_http_failure_is_retried_once() {
    let capture = Capture::default();
    let seen = capture.clone();
    let router = Router::new().route(
        "/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let seen = seen.clone();
            async move {
                seen.record(&headers, &body);
                if seen.attempts() == 1 {
                    (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        Json(json!({"error":"busy"})),
                    )
                } else {
                    (
                        axum::http::StatusCode::OK,
                        Json(response("ok", json!(0.001))),
                    )
                }
            }
        }),
    );
    let port = serve(router).await;
    let client = MachineLlmClient::new(
        "test-key",
        format!("http://127.0.0.1:{port}/chat/completions"),
    );
    let result = client
        .chat(&ChatRequest::new("model").user("hi"))
        .await
        .unwrap();
    assert_eq!(result.text().as_deref(), Some("ok"));
    assert_eq!(capture.attempts(), 2);
}

#[tokio::test]
async fn permanent_http_failure_is_not_retried() {
    let capture = Capture::default();
    let seen = capture.clone();
    let router = Router::new().route(
        "/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let seen = seen.clone();
            async move {
                seen.record(&headers, &body);
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(json!({"error":"bad request"})),
                )
            }
        }),
    );
    let port = serve(router).await;
    let client = MachineLlmClient::new(
        "test-key",
        format!("http://127.0.0.1:{port}/chat/completions"),
    );
    let error = client
        .chat(&ChatRequest::new("model").user("hi"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, LlmError::Http { status: 400, .. }),
        "{error:?}"
    );
    assert_eq!(capture.attempts(), 1);
}

#[tokio::test]
async fn timeout_is_retried_then_reported() {
    let capture = Capture::default();
    let seen = capture.clone();
    let router = Router::new().route(
        "/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let seen = seen.clone();
            async move {
                seen.record(&headers, &body);
                tokio::time::sleep(Duration::from_millis(250)).await;
                Json(response("late", json!(0.0)))
            }
        }),
    );
    let port = serve(router).await;
    let client = MachineLlmClient::new(
        "test-key",
        format!("http://127.0.0.1:{port}/chat/completions"),
    )
    .with_timeout(Duration::from_millis(30));
    let error = client
        .chat(&ChatRequest::new("model").user("hi"))
        .await
        .unwrap_err();
    assert!(matches!(error, LlmError::Timeout(_)), "{error:?}");
    assert_eq!(capture.attempts(), 2);
}

#[tokio::test]
async fn works_against_shared_mock_http_stub() {
    let port = serve(sprites_client::mock_http::router()).await;
    let client = MachineLlmClient::new(
        "test-key",
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
    );
    let result = client
        .chat(&ChatRequest::new("model").user("hi"))
        .await
        .unwrap();
    assert_eq!(result.text().as_deref(), Some("{\"ok\":true}"));
    assert_eq!(result.cost_usd(), Some(0.0123));
}

#[test]
fn allowance_parsing_handles_shapes() {
    assert_eq!(parse_allowance(&json!(0.25)), Some(0.25));
    assert_eq!(parse_allowance(&json!("0.75")), Some(0.75));
    assert_eq!(parse_allowance(&json!({"usd": 1.5})), Some(1.5));
    assert_eq!(
        parse_allowance(&json!({"cost": {"amount": 2.5}})),
        Some(2.5)
    );
    assert_eq!(parse_allowance(&Value::Null), None);
}

#[test]
fn image_parts_serialize_for_judge() {
    let request = ChatRequest::new(JUDGE_MODEL).message(ChatMessage {
        role: "user".into(),
        content: MessageContent::Parts(vec![
            ContentPart::text("rank these"),
            ContentPart::image("data:image/png;base64,AAAA"),
        ]),
    });
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(
        value["messages"][0]["content"][1]["type"],
        json!("image_url")
    );
    assert_eq!(
        value["messages"][0]["content"][1]["image_url"]["url"],
        json!("data:image/png;base64,AAAA")
    );
}
