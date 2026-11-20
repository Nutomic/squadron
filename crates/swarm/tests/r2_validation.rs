#![allow(clippy::unwrap_used)] // test code: panics on failure are the assertion mechanism

//! R2-3 gate tests: API validation (400s), brief cap, run-id gating (404),
//! and static-asset Cache-Control preservation.
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use orchestrator::PipelineConfig;
use sprites_client::SpriteClient;
use tower::ServiceExt;

use swarm::{router, AppState};

const STATIC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/static");

async fn app() -> axum::Router {
    let store = store::Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    let state = AppState::new(store, SpriteClient::from_env(), PipelineConfig::default())
        .with_static_dir(PathBuf::from(STATIC_DIR))
        .without_pipeline();
    router(Arc::new(state))
}

async fn body_string(body: Body) -> String {
    String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

async fn post_json(app: &axum::Router, json: &str) -> axum::http::Response<Body> {
    let req = Request::builder()
        .method("POST")
        .uri("/api/runs")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json.to_owned()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

/// R2-3#5: out-of-range n_lanes (valid range is 3..=10) is a 400 with a JSON
/// error body, never a 500.
#[tokio::test]
async fn api_rejects_out_of_range_n_lanes_with_400() {
    let app = app().await;
    for n in [0usize, 2, 11] {
        let res = post_json(&app, &format!(r#"{{"brief":"x site","n_lanes":{n}}}"#)).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "n_lanes={n}");
        let value: serde_json::Value = serde_json::from_str(&body_string(res.into_body()).await)
            .expect("error body must be JSON");
        assert!(value["error"].as_str().unwrap().contains("n_lanes"));
    }
    // in-range value still works
    let res = post_json(&app, r#"{"brief":"x site","n_lanes":4}"#).await;
    assert_eq!(res.status(), StatusCode::CREATED);
}

/// R2-3#5: briefs are capped (4000 chars) on both POST /api/runs and the web
/// form; briefs are DATA, the cap only bounds payload size.
#[tokio::test]
async fn brief_over_cap_rejected_on_api_and_form() {
    let app = app().await;
    let long: String = "x".repeat(4001);
    let res = post_json(
        &app,
        &serde_json::to_string(&serde_json::json!({ "brief": long, "n_lanes": 4 })).unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let value: serde_json::Value =
        serde_json::from_str(&body_string(res.into_body()).await).unwrap();
    assert!(value["error"].as_str().unwrap().contains("brief"));

    // exactly at the cap is accepted
    let at_cap: String = "y".repeat(4000);
    let res = post_json(
        &app,
        &serde_json::to_string(&serde_json::json!({ "brief": at_cap, "n_lanes": 4 })).unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CREATED);

    // web form rejects over-cap too
    let req = Request::builder()
        .method("POST")
        .uri("/runs")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("brief={}", "x".repeat(4001))))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

/// R2-3#3: every {run_id} extractor is gated through the shared validator:
/// malformed ids get 404 (web pages and API), not 500 or a pass-through.
#[tokio::test]
async fn invalid_run_id_is_404_everywhere() {
    let app = app().await;
    let bad = "way-too-short";
    for uri in [
        format!("/api/runs/{bad}"),
        format!("/runs/{bad}"),
        format!("/{bad}"),
        format!("/{bad}/events"),
        format!("/{bad}/lane/0"),
        format!("/{bad}/thumb/0"),
        format!("/{bad}/source/0"),
        format!("/{bad}/lane/0/tokei"),
    ] {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "GET {uri}");
    }
    // promote is POST; it must also gate
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/{bad}/promote"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

/// R2-3#2: static assets keep the handler's max-age (not clobbered by the
/// security middleware's no-store), while dynamic routes stay no-store.
#[tokio::test]
async fn static_assets_keep_max_age_and_pages_stay_no_store() {
    let app = app().await;
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/static/island.css")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cc = res
        .headers()
        .get(header::CACHE_CONTROL)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(cc.contains("max-age"), "static must be cacheable: {cc}");
    assert!(
        !cc.contains("no-store"),
        "static must not be no-store: {cc}"
    );

    let res = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let cc = res
        .headers()
        .get(header::CACHE_CONTROL)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(cc.contains("no-store"), "pages must stay no-store: {cc}");
}

/// Audit R3 finding: an empty or whitespace-only brief is a 400 on both the
/// JSON API and the web form (parity), and the API stores the trimmed brief.
#[tokio::test]
async fn empty_brief_rejected_on_api_and_form() {
    let app = app().await;
    for json in [
        r#"{"brief":"","n_lanes":4}"#,
        r#"{"brief":"   \n\t ","n_lanes":4}"#,
    ] {
        let res = post_json(&app, json).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "json={json}");
        let value: serde_json::Value =
            serde_json::from_str(&body_string(res.into_body()).await).unwrap();
        assert!(
            value["error"]
                .as_str()
                .unwrap()
                .contains("must not be empty"),
            "{json}"
        );
    }

    let req = Request::builder()
        .method("POST")
        .uri("/runs")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("brief="))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // A padded-but-non-empty brief is accepted (and trimmed server-side).
    let res = post_json(&app, r#"{"brief":"  padded site  ","n_lanes":4}"#).await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let value: serde_json::Value =
        serde_json::from_str(&body_string(res.into_body()).await).unwrap();
    assert!(
        !value["run_id"].as_str().unwrap().is_empty(),
        "run created for a whitespace-padded brief"
    );
}
