use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use axum::{Json, Router};
use futures::stream::Stream;
use orchestrator::snapshot::{self, RunSnapshot};
use orchestrator::{
    HarnessMode, PipelineConfig, PipelineCtx, RunRequest, ScreenshotQueue, SlotSemaphore, SseHub,
};
use serde::{Deserialize, Serialize};
use sprites_client::SpriteClient;
use store::Store;
use tokio::sync::RwLock;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub sprite: SpriteClient,
    pub machine_llm: Option<Arc<llm_gateway::MachineLlmClient>>,
    pub screenshots: ScreenshotQueue,
    pub config: PipelineConfig,
    pub slots: SlotSemaphore,
    pub hubs: Arc<RwLock<HashMap<String, SseHub>>>,
}

impl AppState {
    pub fn new(store: Store, sprite: SpriteClient, config: PipelineConfig) -> Self {
        let screenshots = ScreenshotQueue::new(store.clone(), config.test_mode);
        let slots = SlotSemaphore::new(config.max_running_slots);
        Self {
            store,
            sprite,
            machine_llm: None,
            screenshots,
            config,
            slots,
            hubs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn with_machine_llm(mut self, llm: llm_gateway::MachineLlmClient) -> Self {
        self.machine_llm = Some(Arc::new(llm));
        self
    }

    async fn get_hub(&self, run_id: &str) -> Option<SseHub> {
        self.hubs.read().await.get(run_id).cloned()
    }
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

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/runs", get(list_runs).post(create_run))
        .route("/runs/{run_id}", get(get_run))
        .route("/{run_id}", get(get_run))
        .route("/{run_id}/events", get(run_events))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state)
}

async fn create_run(
    State(state): State<AppState>,
    Json(body): Json<CreateRunBody>,
) -> Result<(StatusCode, Json<CreateRunResponse>), (StatusCode, String)> {
    let n = body.n_lanes.unwrap_or(state.config.n_lanes);
    if !(3..=10).contains(&n) {
        return Err((
            StatusCode::BAD_REQUEST,
            "n_lanes must be in range 3..=10".to_owned(),
        ));
    }
    let harness = match (body.impl_command, body.val_command) {
        (Some(i), Some(v)) => HarnessMode::Command {
            impl_command: i,
            val_command: v,
        },
        _ => HarnessMode::Real {
            gateway_connector_id: state.config.gateway_connector_id.clone(),
        },
    };

    let hub = SseHub::new(256);
    let ctx = Arc::new(PipelineCtx {
        store: state.store.clone(),
        sprite: state.sprite.clone(),
        machine_llm: state.machine_llm.clone(),
        sse: hub.clone(),
        screenshots: state.screenshots.clone(),
        config: state.config.clone(),
        slots: state.slots.clone(),
        hub_registry: Some(state.hubs.clone()),
    });
    let req = RunRequest {
        brief: body.brief,
        n_lanes: n,
        harness,
    };
    let run = orchestrator::start_run(ctx, req).await.map_err(internal)?;

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
    State(state): State<AppState>,
) -> Result<Json<Vec<store::Run>>, (StatusCode, String)> {
    state.store.list_runs().await.map(Json).map_err(internal)
}

async fn get_run(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Json<RunSnapshot>, (StatusCode, String)> {
    let run = state
        .store
        .get_run(&run_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "run not found".to_owned()))?;
    let lanes = state.store.list_lanes(&run_id).await.map_err(internal)?;
    let snap = snapshot::snapshot_for(&run, &lanes, orchestrator::config::now_ms());
    Ok(Json(snap))
}

async fn run_events(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, (StatusCode, String)>
{
    let run = state
        .store
        .get_run(&run_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "run not found".to_owned()))?;
    let lanes = state.store.list_lanes(&run_id).await.map_err(internal)?;
    let initial = snapshot::snapshot_for(&run, &lanes, orchestrator::config::now_ms());

    let hub = state
        .get_hub(&run_id)
        .await
        .unwrap_or_else(|| SseHub::new(16));
    let rx = hub.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|msg| match msg {
        Ok(snap) => Some(Ok(Event::default()
            .json_data(snap)
            .unwrap_or_else(|_| Event::default().data("{}")))),
        Err(_) => None,
    });
    let init_stream = tokio_stream::once(Ok(Event::default()
        .json_data(initial)
        .unwrap_or_else(|_| Event::default().data("{}"))));
    let combined = init_stream.chain(stream);

    Ok(Sse::new(combined).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("heartbeat"),
    ))
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
