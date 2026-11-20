use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use orchestrator::judge;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

use llm_gateway::MachineLlmClient;

/// Stub inference endpoint: records the request body and replies with a
/// schema-valid judge verdict.
async fn stub(
    State(captured): State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    captured.lock().await.push(body.clone());
    (
        StatusCode::OK,
        Json(json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": json!({
                "ranking": [0], "winner": 0,
                "verdict_card": "Lane 0 wins; criteria met.",
                "reasons": "only lane"
            }).to_string()}}],
            "usage": {"opencode": {"allowance": {"usd": 0.01}}}
        })),
    )
}

fn app(captured: Arc<Mutex<Vec<Value>>>) -> Router {
    Router::new()
        .route("/chat/completions", post(stub))
        .with_state(captured)
}

async fn spawn_stub() -> (String, Arc<Mutex<Vec<Value>>>) {
    let captured: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let server_state = captured.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(server_state)).await;
    });
    (format!("http://{addr}/chat/completions"), captured)
}

/// #12: screenshots attached to the judge arrive as image_url parts.
#[tokio::test(flavor = "multi_thread")]
async fn judge_sends_screenshot_image_parts_when_available() {
    let (endpoint, captured) = spawn_stub().await;
    let client = MachineLlmClient::new("test-key", &endpoint);

    let report = prompts::LaneJudgeReport::new(
        0,
        prompts::ValidatorOutput {
            verdict: prompts::Verdict::Pass,
            summary: "ok".to_owned(),
            features: prompts::TextOrList::Text("feature".to_owned()),
            bugs: prompts::TextOrList::Text(String::new()),
            security: prompts::TextOrList::Text(String::new()),
            quality_score: 8.0,
        },
    );
    let png = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../swarm/static/logo.png"),
    )
    .ok()
    .unwrap_or_else(|| vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3]);
    let data_url = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&png)
    );

    let (out, _cost) = judge::run_judge(
        &client,
        &[report.clone()],
        &["<extract>".to_owned()],
        &[Some(data_url.clone())],
        std::time::Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(out.winner, 0);

    let bodies = captured.lock().await;
    let body = &bodies[0];
    let content = body["messages"][1]["content"].as_array().unwrap();
    assert!(
        content
            .iter()
            .any(|p| p["type"] == "image_url" && p["image_url"]["url"] == json!(data_url)),
        "image part must be sent"
    );
    assert!(content.iter().any(|p| p["type"] == "text"));
    assert!(
        content[0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("validator-reports"),
        "judge prompt rides in the text part"
    );
}

/// #12: without screenshots the judge falls back to the plain text request.
#[tokio::test(flavor = "multi_thread")]
async fn judge_without_images_uses_text_only_request() {
    let (endpoint, captured) = spawn_stub().await;
    let client = MachineLlmClient::new("test-key", &endpoint);

    let report = prompts::LaneJudgeReport::new(
        0,
        prompts::ValidatorOutput {
            verdict: prompts::Verdict::Pass,
            summary: "ok".to_owned(),
            features: prompts::TextOrList::Text("feature".to_owned()),
            bugs: prompts::TextOrList::Text(String::new()),
            security: prompts::TextOrList::Text(String::new()),
            quality_score: 8.0,
        },
    );
    let (out, _cost) = judge::run_judge(
        &client,
        &[report],
        &["<extract>".to_owned()],
        &[None],
        std::time::Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(out.winner, 0);

    let bodies = captured.lock().await;
    assert!(bodies[0]["messages"][1]["content"].is_string());
}

/// #24: the judge may only crown a winner from the validated lane set; a
/// verdict naming a lane outside the reports is rejected by the gate.
#[tokio::test(flavor = "multi_thread")]
async fn judge_winner_outside_validated_lanes_is_rejected() {
    async fn rogue_stub(Json(_body): Json<Value>) -> (StatusCode, Json<Value>) {
        (
            StatusCode::OK,
            Json(json!({
                "choices": [{"index": 0, "message": {"role": "assistant", "content": json!({
                    "ranking": [0, 9], "winner": 9,
                    "verdict_card": "Lane 9 wins; criteria met.",
                    "reasons": "rogue lane"
                }).to_string()}}],
                "usage": {"opencode": {"allowance": {"usd": 0.01}}}
            })),
        )
    }
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route("/chat/completions", post(rogue_stub));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = MachineLlmClient::new("test-key", format!("http://{addr}/chat/completions"));

    let reports: Vec<prompts::LaneJudgeReport> = [0usize, 1]
        .iter()
        .map(|idx| {
            prompts::LaneJudgeReport::new(
                *idx,
                prompts::ValidatorOutput {
                    verdict: prompts::Verdict::Pass,
                    summary: "ok".to_owned(),
                    features: prompts::TextOrList::Text("feature".to_owned()),
                    bugs: prompts::TextOrList::Text(String::new()),
                    security: prompts::TextOrList::Text(String::new()),
                    quality_score: 8.0,
                },
            )
        })
        .collect();
    let extracts = vec!["<extract>".to_owned(), "<extract>".to_owned()];
    let images = vec![None, None];

    let err = judge::run_judge(
        &client,
        &reports,
        &extracts,
        &images,
        std::time::Duration::from_secs(10),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("permutation"),
        "rejection must name the permutation gate, got: {err}"
    );
}
