use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::snapshot;
use crate::web::SharedState;

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

async fn create_run(
    State(state): State<SharedState>,
    Json(body): Json<CreateRunBody>,
) -> Result<(StatusCode, Json<CreateRunResponse>), (StatusCode, Json<serde_json::Value>)> {
    // R2-3#5: validation failures are client errors (400 + JSON), not 500.
    if body.brief.chars().count() > MAX_BRIEF_CHARS {
        return Err(bad_request(&format!(
            "brief exceeds {MAX_BRIEF_CHARS} characters"
        )));
    }
    let n = body.n_lanes.unwrap_or(state.config.n_lanes);
    if let Err(e) = orchestrator::PipelineConfig::validate_n(n) {
        return Err(bad_request(&e));
    }
    let run = state
        .start_pipeline(body.brief, n, state.real_harness())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e })),
            )
        })?;
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

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
