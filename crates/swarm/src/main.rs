use std::sync::Arc;

use swarm::{router, static_dir, AppState};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let db_url =
        std::env::var("SWARM_DB_URL").unwrap_or_else(|_| "sqlite:swarm.db?mode=rwc".to_owned());
    let store = store::Store::connect_with_migrations(&db_url)
        .await
        .expect("failed to open database");
    let fixtures = std::env::var("SWARM_FIXTURES").as_deref() == Ok("1");
    let app = router(Arc::new(AppState {
        store,
        fixtures,
        static_dir: static_dir(),
    }));
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("failed to bind");
    tracing::info!("swarm dashboard listening on 0.0.0.0:{port}");
    axum::serve(listener, app).await.expect("server error");
}
