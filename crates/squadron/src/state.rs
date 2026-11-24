use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use orchestrator::{
    HarnessMode, LaneQueue, PipelineConfig, PipelineCtx, RunRequest, ScreenshotQueue,
    SlotSemaphore, SpriteCreateThrottle, SseHub,
};
use serde_json::Value;
use sprites_client::SpriteClient;
use store::{Run, Store};
use tokio::sync::RwLock;

use crate::ratelimit::{RunLimiter, RunLimits};

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub sprite: SpriteClient,
    pub machine_llm: Option<Arc<llm_gateway::MachineLlmClient>>,
    pub screenshots: ScreenshotQueue,
    pub config: PipelineConfig,
    pub slots: SlotSemaphore,
    /// Wave D: the process-wide FIFO lane-slot queue; queued runs are
    /// promoted as global lane slots free.
    pub lane_queue: Arc<LaneQueue>,
    /// F26: the process-wide sprite-create throttle (shared across all runs
    /// and lanes; one create per `sprite_create_spacing`).
    pub create_throttle: Arc<SpriteCreateThrottle>,
    pub hubs: Arc<RwLock<HashMap<String, SseHub>>>,
    pub fixtures: bool,
    pub static_dir: PathBuf,
    pub launch_pipeline: bool,
    /// F24: the harness used by the case-driven entries (`start_pipeline_cases`
    /// and the startup queued-run pickup). Always the real harness in
    /// production; overridable for tests (the harness mode is not persisted,
    /// so a picked-up run can only resume with the state's own harness).
    pub pipeline_harness: HarnessMode,
    /// Review F3: bounded anonymous run creation (per-IP + process caps).
    pub limiter: RunLimiter,
}

impl AppState {
    pub fn new(store: Store, sprite: SpriteClient, config: PipelineConfig) -> Self {
        // The screenshot queue shares the hub registry so thumbnail state
        // changes broadcast to live SSE subscribers (audit R3 finding 3).
        let hubs: Arc<RwLock<HashMap<String, SseHub>>> = Arc::new(RwLock::new(HashMap::new()));
        let screenshots = ScreenshotQueue::with_data_dir_hubs(
            store.clone(),
            config.test_mode,
            config.data_dir.clone(),
            Some(hubs.clone()),
        );
        let slots = SlotSemaphore::new(config.max_running_slots);
        let lane_queue = Arc::new(LaneQueue::new());
        orchestrator::queue::spawn_dispatcher(lane_queue.clone());
        let create_throttle = Arc::new(SpriteCreateThrottle::new(config.sprite_create_spacing));
        // F24: the case-driven entries default to the state's real harness.
        let pipeline_harness = HarnessMode::Real {
            gateway_connector_id: config.gateway_connector_id.clone(),
        };
        Self {
            store,
            sprite,
            machine_llm: None,
            screenshots,
            config,
            slots,
            lane_queue,
            create_throttle,
            hubs,
            fixtures: false,
            static_dir: crate::static_dir(),
            launch_pipeline: true,
            pipeline_harness,
            limiter: RunLimiter::new(RunLimits::from_env()),
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

    /// TEST-ONLY harness override for the case-driven entries (the confirm
    /// path and the queued-run pickup always run the real harness in
    /// production). Never constructed from untrusted input.
    pub fn with_pipeline_harness(mut self, harness: HarnessMode) -> Self {
        self.pipeline_harness = harness;
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
        let ctx = self.build_ctx();
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

    /// F24: case-driven start for the confirm dialog — the SUBMITTED (edited)
    /// cases drive the pipeline, with no decomposition in between. Builds the
    /// same context as `start_pipeline` and enters the same execution
    /// machinery (`orchestrator::start_run_cases`): run row + one lane per
    /// case, slot-accounted (fits → lanes start; otherwise the run is queued
    /// for FIFO promotion). Without the pipeline (dev/test), rows-only
    /// persistence of the submitted cases — the old confirm behavior.
    pub async fn start_pipeline_cases(
        &self,
        brief: String,
        cases: Vec<Value>,
    ) -> Result<Run, String> {
        let harness = self.pipeline_harness.clone();
        self.start_pipeline_cases_with_harness(brief, cases, harness)
            .await
    }

    /// TEST-ONLY harness override for `start_pipeline_cases` (the web confirm
    /// always uses the real harness). Never constructed from untrusted input.
    pub async fn start_pipeline_cases_with_harness(
        &self,
        brief: String,
        cases: Vec<Value>,
        harness: HarnessMode,
    ) -> Result<Run, String> {
        if cases.is_empty() {
            return Err("at least one case is required".to_owned());
        }
        if !self.launch_pipeline {
            return self.create_queued_run_from_cases(&brief, &cases).await;
        }
        let ctx = self.build_ctx();
        orchestrator::start_run_cases(ctx, &brief, cases, harness, None).await
    }

    /// Shared per-run pipeline context: a fresh SSE hub (registered under the
    /// run id by the pipeline), shared store/sprite/slots/screenshots/queue.
    fn build_ctx(&self) -> Arc<PipelineCtx> {
        let hub = SseHub::new(256);
        Arc::new(PipelineCtx {
            store: self.store.clone(),
            sprite: self.sprite.clone(),
            machine_llm: self.machine_llm.clone(),
            sse: hub,
            screenshots: self.screenshots.clone(),
            config: self.config.clone(),
            slots: self.slots.clone(),
            harness_cache: std::sync::Arc::new(orchestrator::harness::HarnessCache::new()),
            hub_registry: Some(self.hubs.clone()),
            lane_queue: Some(self.lane_queue.clone()),
            create_throttle: self.create_throttle.clone(),
        })
    }

    /// F24: crash-safe queue pickup. `LaneQueue` is in-memory, so runs left in
    /// `queued` status by a process restart (crash or deploy) would stay
    /// stranded forever: the DB says queued but nothing re-enqueues them.
    /// Called once at startup, before the server accepts requests: every
    /// DB-queued run is rebuilt into a `QueuedRun` and handed to the FIFO
    /// dispatcher, which promotes it with the same slot accounting as any
    /// freshly queued run (MAX lanes cap respected at promote time). Returns
    /// the number of runs re-enqueued; each is logged.
    pub async fn pickup_queued_runs(&self) -> usize {
        if !self.launch_pipeline {
            return 0;
        }
        let stranded = match self.store.list_queued_runs().await {
            Ok(runs) => runs,
            Err(e) => {
                tracing::warn!("queued-run pickup scan failed: {e}");
                return 0;
            }
        };
        let mut picked = 0usize;
        for run in stranded {
            let lanes = match self.store.list_lanes(&run.id).await {
                Ok(lanes) => lanes,
                Err(e) => {
                    tracing::warn!(
                        "queued-run pickup: skipping run {} (cannot list lanes: {e})",
                        run.id
                    );
                    continue;
                }
            };
            if lanes.is_empty() {
                // Rows were never written (crash between run and lane
                // inserts): nothing to rebuild — leave it queued with a loud
                // log rather than enqueue a run the dispatcher cannot run.
                tracing::warn!(
                    "queued-run pickup: run {} has no lanes; cannot rebuild, leaving it queued",
                    run.id
                );
                continue;
            }
            let ctx = self.build_ctx();
            if let Some(reg) = &ctx.hub_registry {
                reg.write().await.insert(run.id.clone(), ctx.sse.clone());
            }
            self.lane_queue.enqueue(orchestrator::queue::QueuedRun {
                ctx,
                run_id: run.id.clone(),
                lanes: lanes.len(),
                harness: self.pipeline_harness.clone(),
                decompose_cost: None,
            });
            tracing::info!(
                "queued-run pickup: re-enqueued run {} ({} lanes) after restart; waiting for FIFO promotion",
                run.id,
                lanes.len()
            );
            picked += 1;
        }
        picked
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

    /// F24: rows-only persistence of the submitted cases (the
    /// `launch_pipeline = false` dev/test path): the same shape the old
    /// confirm path persisted — one queued lane per case, the case JSON in
    /// `variant_json`.
    async fn create_queued_run_from_cases(
        &self,
        brief: &str,
        cases: &[Value],
    ) -> Result<Run, String> {
        let now = store::now_ms();
        let run = self
            .store
            .create_run(brief, cases.len() as i64, now)
            .await
            .map_err(|e| e.to_string())?;
        for (idx, case) in cases.iter().enumerate() {
            let vjson = serde_json::to_string(case).unwrap_or_else(|_| "{}".to_owned());
            self.store
                .insert_lane(&run.id, idx as i64, &vjson)
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
