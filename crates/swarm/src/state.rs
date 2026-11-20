use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use orchestrator::{
    HarnessMode, PipelineConfig, PipelineCtx, RunRequest, ScreenshotQueue, SlotSemaphore, SseHub,
};
use sprites_client::SpriteClient;
use store::{Run, Store};
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub sprite: SpriteClient,
    pub machine_llm: Option<Arc<llm_gateway::MachineLlmClient>>,
    pub screenshots: ScreenshotQueue,
    pub config: PipelineConfig,
    pub slots: SlotSemaphore,
    pub hubs: Arc<RwLock<HashMap<String, SseHub>>>,
    pub fixtures: bool,
    pub static_dir: PathBuf,
    pub launch_pipeline: bool,
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
            fixtures: false,
            static_dir: crate::static_dir(),
            launch_pipeline: true,
        }
    }

    pub fn with_machine_llm(mut self, llm: llm_gateway::MachineLlmClient) -> Self {
        self.machine_llm = Some(Arc::new(llm));
        self
    }

    pub fn with_fixtures(mut self, fixtures: bool) -> Self {
        self.fixtures = fixtures;
        self
    }

    pub fn with_static_dir(mut self, dir: PathBuf) -> Self {
        self.static_dir = dir;
        self
    }

    pub fn without_pipeline(mut self) -> Self {
        self.launch_pipeline = false;
        self
    }

    pub fn real_harness(&self) -> HarnessMode {
        HarnessMode::Real {
            gateway_connector_id: self.config.gateway_connector_id.clone(),
        }
    }

    pub async fn get_hub(&self, run_id: &str) -> Option<SseHub> {
        self.hubs.read().await.get(run_id).cloned()
    }

    pub async fn start_pipeline(
        &self,
        brief: String,
        n_lanes: usize,
        harness: HarnessMode,
    ) -> Result<Run, String> {
        PipelineConfig::validate_n(n_lanes)?;
        if !self.launch_pipeline {
            return self.create_queued_run(&brief, n_lanes).await;
        }
        let hub = SseHub::new(256);
        let ctx = Arc::new(PipelineCtx {
            store: self.store.clone(),
            sprite: self.sprite.clone(),
            machine_llm: self.machine_llm.clone(),
            sse: hub,
            screenshots: self.screenshots.clone(),
            config: self.config.clone(),
            slots: self.slots.clone(),
            harness_cache: std::sync::Arc::new(orchestrator::harness::HarnessCache::new()),
            hub_registry: Some(self.hubs.clone()),
        });
        orchestrator::start_run(
            ctx,
            RunRequest {
                brief,
                n_lanes,
                harness,
            },
        )
        .await
    }

    async fn create_queued_run(&self, brief: &str, n_lanes: usize) -> Result<Run, String> {
        let now = store::now_ms();
        let run = self
            .store
            .create_run(brief, n_lanes as i64, now)
            .await
            .map_err(|e| e.to_string())?;
        for idx in 0..n_lanes {
            self.store
                .insert_lane(&run.id, idx as i64, "{}")
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(run)
    }
}
