use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

use crate::ratelimit::Rejection;
use crate::snapshot;
use crate::web::{client_ip, SharedState};

/// R2-3#5: briefs are bounded so a runaway client cannot submit unbounded
/// payloads; the cap is shared with the web form handler.
pub const MAX_BRIEF_CHARS: usize = 4000;

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/api/runs", get(list_runs).post(create_run))
        .route("/api/runs/{run_id}", get(get_run))
        .route("/runs/{run_id}", get(get_run))
        .route("/healthz", get(healthz))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
pub struct CreateRunBody {
    pub brief: String,
    #[serde(default)]
    pub n_lanes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct CreateRunResponse {
    pub run_id: String,
    pub status: String,
    pub n_lanes: i64,
}

// Axum handlers return the full Response type; the variant size is inherent
// to the framework, not a design issue worth boxing at this call site.
#[allow(clippy::result_large_err)]
async fn create_run(
    State(state): State<SharedState>,
    headers: HeaderMap,
    ConnectInfo(info): ConnectInfo<SocketAddr>,
    Json(body): Json<CreateRunBody>,
) -> Result<(StatusCode, Json<CreateRunResponse>), axum::response::Response> {
    // R2-3#5: validation failures are client errors (400 + JSON), not 500.
    // Audit R3 finding: web-form parity — an empty or whitespace-only brief is
    // a 400, not a run (brief is DATA; only whitespace normalization happens).
    let brief = body.brief.trim().to_owned();
    if brief.is_empty() {
        return Err(bad_request("brief must not be empty").into_response());
    }
    if brief.chars().count() > MAX_BRIEF_CHARS {
        return Err(
            bad_request(&format!("brief exceeds {MAX_BRIEF_CHARS} characters")).into_response(),
        );
    }
    let n = body.n_lanes.unwrap_or(state.config.n_lanes);
    if let Err(e) = orchestrator::PipelineConfig::validate_n(n) {
        return Err(bad_request(&e).into_response());
    }
    // Review F3 #1/#2: bounded anonymous run creation (CSRF does not apply to
    // the JSON API; the rate limits do).
    let ip = client_ip(&headers, ConnectInfo(info));
    if let Err(rejection) = state.limiter.check(&ip, &state.store).await {
        return Err(rejected(&rejection));
    }
    let run = state
        .start_pipeline(brief, n, state.real_harness())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e })),
            )
                .into_response()
        })?;
    state.limiter.record(&ip, &run.id);
    Ok((
        StatusCode::CREATED,
        Json(CreateRunResponse {
            run_id: run.id.clone(),
            status: run.status,
            n_lanes: run.n_lanes,
        }),
    ))
}

async fn list_runs(
    State(state): State<SharedState>,
) -> Result<Json<Vec<store::Run>>, (StatusCode, String)> {
    state.store.list_runs().await.map(Json).map_err(internal)
}

async fn get_run(
    State(state): State<SharedState>,
    Path(run_id): Path<String>,
) -> Result<Json<snapshot::Snapshot>, (StatusCode, String)> {
    // R2-3#3: every {run_id} extractor is gated through the shared helper.
    if !crate::web::valid_id(&run_id) {
        return Err((StatusCode::NOT_FOUND, "run not found".to_owned()));
    }
    snapshot::build_snapshot(&state.store, &run_id)
        .await
        .map(Json)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "run not found".to_owned()))
}

async fn healthz() -> &'static str {
    "ok"
}

fn bad_request(msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg })),
    )
}

/// Review F3: 429 + Retry-After when a rate limit refuses the creation.
fn rejected(rejection: &Rejection) -> axum::response::Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(axum::http::header::RETRY_AFTER, "60")],
        Json(serde_json::json!({ "error": rejection.message() })),
    )
        .into_response()
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
