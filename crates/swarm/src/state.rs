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
        let screenshots = ScreenshotQueue::with_data_dir(
            store.clone(),
            config.test_mode,
            config.data_dir.clone(),
        );
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

    /// R2 finding 1: construct the machine judge client when OPENCODE_KEY is
    /// set. Without the key, log a clear warning and keep the deterministic
    /// fallback judge (dev/test only); production must never fabricate
    /// verdicts, so it requires the key.
    pub fn machine_llm_from_env() -> Option<llm_gateway::MachineLlmClient> {
        match llm_gateway::MachineLlmClient::from_env() {
            Ok(llm) => Some(llm),
            Err(_) => {
                tracing::warn!(
                    "OPENCODE_KEY not set; judge will use the deterministic fallback (dev/test only). \
                     Set OPENCODE_KEY to enable the real machine judge."
                );
                None
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

    /// R2 finding 1: the machine judge is only constructed when OPENCODE_KEY is
    /// present; without it the deterministic fallback stays in place.
    #[test]
    fn machine_llm_from_env_requires_opencode_key() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("OPENCODE_KEY");
        assert!(AppState::machine_llm_from_env().is_none());
        std::env::set_var("OPENCODE_KEY", "test-only-key");
        assert!(AppState::machine_llm_from_env().is_some());
        std::env::remove_var("OPENCODE_KEY");
        assert!(AppState::machine_llm_from_env().is_none());
    }
}
