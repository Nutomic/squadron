#![allow(clippy::unwrap_used, clippy::panic)] // test code: panics on failure are the assertion mechanism

//! Review F3: bounded anonymous run creation + CSRF double-submit tokens.
//! The store is the in-memory mock; no real sprites are launched
//! (launch_pipeline disabled).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use orchestrator::PipelineConfig;
use sprites_client::SpriteClient;
use store::Store;
use tower::ServiceExt;

use squadron::{router, AppState};

const STATIC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/static");

const MOCK_CONNECT_INFO: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 1234);

async fn app() -> (axum::Router, Store) {
    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    let state = AppState::new(
        store.clone(),
        SpriteClient::from_env(),
        PipelineConfig::default(),
    )
    .with_static_dir(PathBuf::from(STATIC_DIR))
    .without_pipeline();
    let app = router(Arc::new(state)).layer(axum::extract::connect_info::MockConnectInfo(
        MOCK_CONNECT_INFO,
    ));
    (app, store)
}

async fn body_string(body: Body) -> String {
    String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

/// GET / issues an unguessable double-submit token in the swarm_csrf cookie.
#[tokio::test]
async fn form_get_sets_unguessable_csrf_cookie() {
    let (app, _) = app().await;
    let mut tokens = Vec::new();
    for _ in 0..2 {
        let res = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let cookie = res
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .expect("GET / must set swarm_csrf")
            .to_owned();
        assert!(cookie.starts_with("swarm_csrf="), "cookie: {cookie}");
        assert!(cookie.contains("SameSite=Lax") && cookie.contains("HttpOnly"));
        tokens.push(
            cookie["swarm_csrf=".len()..]
                .split(';')
                .next()
                .unwrap()
                .to_owned(),
        );
    }
    assert_eq!(tokens.len(), 2);
    assert_ne!(tokens[0], tokens[1], "tokens must be fresh and unguessable");
    assert_eq!(tokens[0].len(), 32);
    // The page carries the same token for the double submit.
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let html = body_string(res.into_body()).await;
    assert!(html.contains("csrf_token"), "hidden double-submit field");
}

/// POST /runs (confirm) without (or with a mismatching) token is 403; a
/// valid cookie+token pair creates the run from the submitted cases.
#[tokio::test]
async fn confirm_requires_matching_csrf_token() {
    let (app, store) = app().await;
    let cases = r#"{"brief":"x site","cases":[{"label":"a","impl_prompt":"p"}]}"#;
    // no token at all
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/runs")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(cases))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    // mismatching cookie/token pair
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/runs")
                .header(header::CONTENT_TYPE, "application/json")
                .header(
                    header::COOKIE,
                    "swarm_csrf=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                )
                .body(Body::from(
                    r#"{"brief":"x site","cases":[{"label":"a","impl_prompt":"p"}],"csrf_token":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert!(store.list_runs().await.unwrap().is_empty());

    // valid pair: token set on GET, echoed in the body, cookie sent back.
    let token = {
        let res = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        res.headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap()
            .strip_prefix("swarm_csrf=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    };
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/runs")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("swarm_csrf={token}"))
                .body(Body::from(format!(
                    r#"{{"brief":"csrf ok","cases":[{{"label":"a","impl_prompt":"p"}}],"csrf_token":"{token}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    assert_eq!(store.list_runs().await.unwrap().len(), 1);
}

/// POST /{run_id}/promote is CSRF-checked too; the JSON API is not.
#[tokio::test]
async fn promote_requires_token_but_json_api_is_unaffected() {
    let (app, store) = app().await;
    let run = store
        .create_run("promote csrf", 2, 1_732_020_000_000)
        .await
        .unwrap();
    store
        .finish_run(&run.id, "judged", "ok", 0, None, 1)
        .await
        .unwrap();
    let token = {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/{}", run.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        res.headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .expect("gallery GET must set swarm_csrf")
            .strip_prefix("swarm_csrf=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    };
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/{}/promote", run.id))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("csrf_token={token}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "cookie missing");
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/{}/promote", run.id))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("swarm_csrf={token}"))
                .body(Body::from(format!("csrf_token={token}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        store.get_run(&run.id).await.unwrap().unwrap().status,
        "promoted"
    );

    // JSON /api/runs is NOT CSRF-checked (cookie-auth-free API).
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runs")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"brief":"api run","n_lanes":2}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
}

/// F3 #2: at the process-wide active-run cap a new creation is rejected with
/// 429 + Retry-After (JSON on both the API and the dialog confirm).
#[tokio::test]
async fn process_active_run_cap_returns_429_with_retry_after() {
    let (app, store) = app().await;
    let ips = ["10.0.0.1", "10.0.0.2", "10.0.0.3", "10.0.0.4"];
    for ip in ips {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/runs")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("x-forwarded-for", ip)
                    .body(Body::from(r#"{"brief":"cap","n_lanes":2}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED, "ip {ip}");
    }
    assert_eq!(store.count_active_runs().await.unwrap(), 4);

    // 5th active run (fresh IP) is refused with 429 + Retry-After.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runs")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-forwarded-for", "10.0.0.5")
                .body(Body::from(r#"{"brief":"cap","n_lanes":2}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        res.headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("60")
    );
    let value: serde_json::Value =
        serde_json::from_str(&body_string(res.into_body()).await).unwrap();
    assert!(value.get("error").is_some());

    // The dialog confirm gets a JSON 429 too (a valid CSRF pair first:
    // CSRF runs before the rate limit).
    let token = {
        let res = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        res.headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap()
            .strip_prefix("swarm_csrf=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    };
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/runs")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("swarm_csrf={token}"))
                .body(Body::from(format!(
                    r#"{{"brief":"confirm cap","cases":[{{"label":"a","impl_prompt":"p"}}],"csrf_token":"{token}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        res.headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("60")
    );
    let value: serde_json::Value =
        serde_json::from_str(&body_string(res.into_body()).await).unwrap();
    assert!(value.get("error").is_some());
}

/// F3 #1: per-IP concurrency — at most 3 non-terminal runs per IP (default);
/// the 4th from the same IP is 429, other IPs are unaffected, and finishing a
/// run frees the slot again.
#[tokio::test]
async fn per_ip_concurrent_run_cap() {
    let (app, store) = app().await;
    for i in 0..3 {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/runs")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("x-forwarded-for", "1.1.1.1")
                    .body(Body::from(format!(r#"{{"brief":"ip{i}","n_lanes":2}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED, "run {i}");
    }
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runs")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-forwarded-for", "1.1.1.1")
                .body(Body::from(r#"{"brief":"4th","n_lanes":2}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    // A different IP is fine.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runs")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-forwarded-for", "2.2.2.2")
                .body(Body::from(r#"{"brief":"other ip","n_lanes":2}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    // Finish one of 1.1.1.1's runs; the slot frees up.
    let runs = store.list_runs().await.unwrap();
    for run in &runs {
        if run.brief == "ip0" {
            store
                .finish_run(&run.id, "judged", "ok", 0, None, 1)
                .await
                .unwrap();
        }
    }
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runs")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-forwarded-for", "1.1.1.1")
                .body(Body::from(r#"{"brief":"4th retry","n_lanes":2}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::CREATED,
        "finished run frees a slot"
    );
}

/// F3 #1: the hourly per-IP creation budget rejects creations beyond the
/// cap even when all earlier runs are terminal (concurrency freed).
#[tokio::test]
async fn per_ip_hourly_creation_cap() {
    let (app, store) = app().await;
    for i in 0..10 {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/runs")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("x-forwarded-for", "3.3.3.3")
                    .body(Body::from(format!(r#"{{"brief":"h{i}","n_lanes":2}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED, "run {i}");
        // Terminalize immediately so only the hourly budget can block.
        let value: serde_json::Value =
            serde_json::from_str(&body_string(res.into_body()).await).unwrap();
        let run_id = value.get("run_id").unwrap().as_str().unwrap().to_owned();
        store
            .finish_run(&run_id, "judged", "ok", 0, None, 1)
            .await
            .unwrap();
    }
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runs")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-forwarded-for", "3.3.3.3")
                .body(Body::from(r#"{"brief":"11th","n_lanes":2}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    // Other IPs still have their budget.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runs")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-forwarded-for", "4.4.4.4")
                .body(Body::from(r#"{"brief":"fresh ip","n_lanes":2}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
}
