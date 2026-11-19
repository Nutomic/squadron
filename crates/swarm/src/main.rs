use std::net::SocketAddr;

use orchestrator::PipelineConfig;
use sprites_client::SpriteClient;
use store::Store;
use swarm::{router, AppState};

#[tokio::main]
async fn main() {
    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "sqlite:/data/swarm.db?mode=rwc".to_owned());
    let store = Store::connect_with_migrations(&db_url).await.expect("db");
    let sprite = SpriteClient::from_env();
    let mut config = PipelineConfig::default();
    if let Ok(dir) = std::env::var("DATA_DIR") {
        config.data_dir = dir.into();
    }
    config.test_mode = std::env::var("SWARM_TEST_MODE").ok().as_deref() == Some("1");
    let state = AppState::new(store, sprite, config);
    let app = router(state);
    let addr: SocketAddr = std::env::var("BIND")
        .unwrap_or_else(|_| "0.0.0.0:3000".to_owned())
        .parse()
        .unwrap();
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
