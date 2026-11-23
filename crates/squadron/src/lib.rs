mod api;
mod csrf;
mod fixture;
mod ratelimit;
mod security;
mod snapshot;
mod state;
mod web;

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::Router;

pub use state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    let web = web::router(state.clone()).fallback(not_found);
    let api = api::router(state.clone());
    let mut app = Router::new().merge(web).merge(api);
    if state.fixtures {
        app = app.merge(fixture::fixture_router());
    }
    app.fallback(not_found)
        .layer(axum::middleware::from_fn(security::harden))
}

async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html("<html><body><h1>404</h1><p><a href=\"/\">home</a></p></body></html>"),
    )
        .into_response()
}

pub fn static_dir() -> PathBuf {
    std::env::var("SQUADRON_STATIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/static")))
}
