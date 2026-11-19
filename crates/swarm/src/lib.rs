mod fixture;
mod security;
mod snapshot;
mod state;
mod web;

use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::Router;
use std::path::PathBuf;
use std::sync::Arc;

pub use state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    let mut app = web::router(state.clone()).fallback(not_found);
    if state.fixtures {
        app = app.merge(fixture::fixture_router());
    }
    app.layer(axum::middleware::from_fn(security::harden))
}

async fn not_found() -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        Html("<html><body><h1>404</h1><p><a href=\"/\">home</a></p></body></html>"),
    )
        .into_response()
}

pub fn static_dir() -> PathBuf {
    std::env::var("SWARM_STATIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/static")))
}
