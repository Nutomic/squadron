use std::net::SocketAddr;
use std::sync::Arc;

use orchestrator::PipelineConfig;
use sprites_client::SpriteClient;
use store::Store;
use swarm::{router, static_dir, AppState};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let db_url = std::env::var("DATABASE_URL")
        .or_else(|_| std::env::var("SWARM_DB_URL"))
        .unwrap_or_else(|_| "sqlite:/data/swarm.db?mode=rwc".to_owned());
    let store = Store::connect_with_migrations(&db_url)
        .await
        .expect("failed to open database");

    let mut config = PipelineConfig::default();
    if let Ok(dir) = std::env::var("DATA_DIR") {
        config.data_dir = dir.into();
    }
    config.test_mode = std::env::var("SWARM_TEST_MODE").as_deref() == Ok("1");

    let state = AppState::new(store, SpriteClient::from_env(), config)
        .with_fixtures(std::env::var("SWARM_FIXTURES").as_deref() == Ok("1"))
        .with_static_dir(static_dir());
    let app = router(Arc::new(state));

    let addr = match std::env::var("BIND") {
        Ok(bind) => bind.parse().expect("invalid BIND address"),
        Err(_) => {
            let port: u16 = std::env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(3000);
            SocketAddr::from(([0, 0, 0, 0], port))
        }
    };
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind");
    tracing::info!("swarm listening on {addr}");
    axum::serve(listener, app).await.expect("server error");
}
