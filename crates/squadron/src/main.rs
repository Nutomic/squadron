use std::net::SocketAddr;
use std::sync::Arc;

use orchestrator::PipelineConfig;
use sprites_client::SpriteClient;
use squadron::{router, static_dir, AppState};
use store::Store;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let db_url = std::env::var("DATABASE_URL")
        .or_else(|_| std::env::var("SQUADRON_DB_URL"))
        .unwrap_or_else(|_| "sqlite:/data/squadron.db?mode=rwc".to_owned());
    let store = Store::connect_with_migrations(&db_url).await?;

    let mut config = PipelineConfig::default();
    if let Ok(dir) = std::env::var("DATA_DIR") {
        config.data_dir = dir.into();
    }
    config.test_mode = std::env::var("SQUADRON_TEST_MODE").as_deref() == Ok("1");
    // Wave D: global lane-slot cap, env-overridable (default 8; org limit is
    // 10 concurrent sprites and headroom matters).
    config.max_running_slots = orchestrator::config::max_concurrent_lanes_from_env();

    let mut state = AppState::new(store, SpriteClient::from_env(), config)
        .with_fixtures(std::env::var("SQUADRON_FIXTURES").as_deref() == Ok("1"))
        .with_static_dir(static_dir());
    // R2 finding 1: the real machine judge when OPENCODE_KEY is set; otherwise
    // machine_llm_from_env logs a warning and the deterministic fallback (only
    // acceptable in dev/test) stays in place.
    if let Some(llm) = squadron::AppState::machine_llm_from_env() {
        state = state.with_machine_llm(llm);
    }
    // F24: crash-safe queue pickup — runs left `queued` in the DB by a
    // restart (crash or deploy) re-enter the FIFO queue before the server
    // accepts requests. This also un-strands runs queued by a previous
    // deploy; each picked-up run is logged.
    state.pickup_queued_runs().await;
    let app = router(Arc::new(state));

    let addr = match std::env::var("BIND") {
        Ok(bind) => bind.parse()?,
        Err(_) => {
            let port: u16 = std::env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(3000);
            SocketAddr::from(([0, 0, 0, 0], port))
        }
    };
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("squadron listening on {addr}");
    // ConnectInfo is needed by the F3 per-IP run-creation rate limit.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
