use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use orchestrator::HarnessMode;
use serde::{Deserialize, Serialize};

use crate::snapshot;
use crate::web::SharedState;

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
    #[serde(default)]
    pub impl_command: Option<String>,
    #[serde(default)]
    pub val_command: Option<String>,
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
) -> Result<(StatusCode, Json<CreateRunResponse>), (StatusCode, String)> {
    let n = body.n_lanes.unwrap_or(state.config.n_lanes);
    let harness = match (body.impl_command, body.val_command) {
        (Some(impl_command), Some(val_command)) => HarnessMode::Command {
            impl_command,
            val_command,
        },
        _ => state.real_harness(),
    };
    let run = state
        .start_pipeline(body.brief, n, harness)
        .await
        .map_err(internal)?;
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
    snapshot::build_snapshot(&state.store, &run_id)
        .await
        .map(Json)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "run not found".to_owned()))
}

async fn healthz() -> &'static str {
    "ok"
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
