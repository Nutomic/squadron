use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

pub fn router() -> Router {
    Router::new()
        .route("/", get(root))
        .route("/healthz", get(healthz))
        .route("/v1/chat/completions", post(chat_completions))
}

async fn root() -> Html<&'static str> {
    Html("<!doctype html><html><body><h1>mock sprite site</h1></body></html>")
}

async fn healthz() -> &'static str {
    "ok"
}

async fn chat_completions(Json(request): Json<Value>) -> Json<Value> {
    let content = std::env::var("MOCK_LLM_CONTENT").unwrap_or_else(|_| "{\"ok\":true}".to_owned());
    let cost: f64 = std::env::var("MOCK_LLM_COST")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0123);
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("mock-model");
    Json(json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15,
            "opencode": { "allowance": cost }
        }
    }))
}
