use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use orchestrator::{
    HarnessMode, PipelineConfig, PipelineCtx, ScreenshotQueue, SlotSemaphore, SseHub,
};
use sprites_client::{SpriteClient, SpriteConfig};
use store::{Lane, Run, Store};

/// Serializes the process-global mock-sprite env vars between pipeline tests
/// that run in the same test binary.
pub static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn canned_impl() -> String {
    serde_json::json!({
        "summary": "Implemented arcade page",
        "files": 3,
        "run_command": "python3 -m http.server 8080",
        "done": true,
        "blockers": []
    })
    .to_string()
}

pub fn canned_val() -> String {
    serde_json::json!({
        "verdict": "pass",
        "features": ["game loop", "score"],
        "bugs": [],
        "security": "no issues found",
        "quality_score": 88.5,
        "summary": "Detailed validation: implementation matches claims and serves content."
    })
    .to_string()
}

pub fn set_mock_env(dir: &tempfile::TempDir) {
    let state_path = dir.path().join("mock-sprites.json");
    std::env::set_var("MOCK_SPRITE_STATE", &state_path);
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");
}

pub fn clear_mock_env() {
    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
    std::env::remove_var("MOCK_CREATE_MS");
    std::env::remove_var("MOCK_EXEC_MS");
}

pub fn mock_sprite_client() -> SpriteClient {
    SpriteClient::new(SpriteConfig {
        cli_path: option_env!("CARGO_BIN_EXE_mock-sprite-cli")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mock-sprite-cli")
            }),
        command_timeout: Duration::from_secs(30),
        ..SpriteConfig::default()
    })
}

pub fn base_config(dir: &tempfile::TempDir) -> PipelineConfig {
    let mut config = PipelineConfig::default();
    config.n_lanes = 3;
    config.max_running_slots = 10;
    config.test_mode = true;
    config.retries_per_lane = 0;
    config.data_dir = dir.path().join("data");
    std::fs::create_dir_all(&config.data_dir).unwrap();
    config.impl_deadline = Duration::from_secs(30);
    config.val_deadline = Duration::from_secs(30);
    config.judge_deadline = Duration::from_secs(15);
    config
}

pub async fn setup_ctx(
    tune: impl FnOnce(&mut PipelineConfig),
) -> (Arc<PipelineCtx>, Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    set_mock_env(&dir);
    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    let mut config = base_config(&dir);
    tune(&mut config);
    let ctx = Arc::new(PipelineCtx {
        store: store.clone(),
        sprite: mock_sprite_client(),
        machine_llm: None,
        sse: SseHub::new(256),
        screenshots: ScreenshotQueue::new(store.clone(), true),
        slots: SlotSemaphore::new(config.max_running_slots),
        config,
        hub_registry: None,
    });
    (ctx, store, dir)
}

pub fn harness(impl_command: &str, val_command: &str) -> HarnessMode {
    HarnessMode::Command {
        impl_command: impl_command.to_owned(),
        val_command: val_command.to_owned(),
    }
}

pub fn sh(quoted: &str) -> String {
    format!("echo '{}'", quoted.replace('\'', "'\\''"))
}

/// Poll until the run reaches a terminal status (`judged` or `failed`).
pub async fn wait_run_done(store: &Store, run_id: &str) -> Run {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let r = store.get_run(run_id).await.unwrap().unwrap();
        if r.status == "judged" || r.status == "failed" {
            return r;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timeout waiting for run {run_id} to finish"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll until every lane of the run left `queued`/`running`.
pub async fn wait_lanes_settled(store: &Store, run_id: &str) -> Vec<Lane> {
    wait_run_done(store, run_id).await;
    store.list_lanes(run_id).await.unwrap()
}
