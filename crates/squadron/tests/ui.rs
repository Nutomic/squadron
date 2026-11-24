#![allow(clippy::unwrap_used, clippy::panic)] // test code: panics on failure are the assertion mechanism

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use futures::StreamExt;
use http_body_util::BodyExt;
use orchestrator::PipelineConfig;
use sprites_client::SpriteClient;
use store::Store;
use tower::ServiceExt;

use squadron::{router, AppState};

const STATIC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/static");

/// The F3 per-IP rate limit reads the socket address through ConnectInfo;
/// oneshot tests mock it (production uses into_make_service_with_connect_info).
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
    .with_fixtures(true)
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

/// Issue a GET that renders a form and return the double-submit CSRF token
/// (from the squadron_csrf Set-Cookie header) — review F3 #3.
async fn csrf_token_for(app: &axum::Router, uri: &str) -> String {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let cookie = res
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .expect("form GET must set the squadron_csrf cookie")
        .to_owned();
    cookie
        .strip_prefix("squadron_csrf=")
        .and_then(|rest| rest.split(';').next())
        .filter(|t| !t.is_empty())
        .expect("squadron_csrf cookie must carry a token")
        .to_owned()
}

/// Wave C: the confirm posts the edited cases as JSON (the dialog flow
/// replaced the form submit); the CSRF token rides in body + cookie.
async fn post_run(app: &axum::Router, body: serde_json::Value) -> axum::http::Response<Body> {
    let token = csrf_token_for(app, "/").await;
    let mut json = body;
    json["csrf_token"] = serde_json::json!(token);
    let req = Request::builder()
        .method("POST")
        .uri("/runs")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, format!("squadron_csrf={token}"))
        .body(Body::from(json.to_string()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

fn cases_json(n: usize, brief: &str) -> serde_json::Value {
    serde_json::json!({
        "brief": brief,
        "cases": (0..n)
            .map(|i| serde_json::json!({"label": format!("w{i}"), "impl_prompt": format!("prompt {i}")}))
            .collect::<Vec<_>>()
    })
}

/// Wave C: the landing page carries the brief textarea, sample chips and the
/// dialog island mount — the n_lanes selector is gone (the dialog owns the
/// lane count).
#[tokio::test]
async fn index_renders_form_chips_and_dialog_island() {
    let (app, _) = app().await;
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res.into_body()).await;
    assert!(html.contains(r#"name="brief""#));
    assert!(html.contains("launch-island"));
    assert!(html.contains("data-csrf-token"));
    assert!(!html.contains(r#"name="n""#), "n_lanes input removed");
    assert!(html.contains("Netscape-era"));
    assert!(html.contains("arcade"));
    assert!(html.contains("VM product"));
}

#[tokio::test]
async fn confirm_creates_run_and_lanes_from_cases() {
    let (app, store) = app().await;
    let res = post_run(&app, cases_json(5, "Build a retro terminal portfolio")).await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let value: serde_json::Value =
        serde_json::from_str(&body_string(res.into_body()).await).unwrap();
    let run_id = value["run_id"].as_str().unwrap().to_owned();
    assert_eq!(value["n_lanes"], serde_json::json!(5));
    let run = store.get_run(&run_id).await.unwrap().unwrap();
    assert_eq!(run.status, "queued");
    assert_eq!(run.n_lanes, 5);
    let lanes = store.list_lanes(&run_id).await.unwrap();
    assert_eq!(lanes.len(), 5);
    assert!(lanes.iter().all(|l| l.state == "queued"));
}

#[tokio::test]
async fn confirm_requires_cases_and_owns_lane_count() {
    let (app, store) = app().await;
    // No cases at all: 400, nothing persisted.
    let res = post_run(&app, serde_json::json!({"brief": "   ", "cases": []})).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(store.list_runs().await.unwrap().is_empty());
    // The dialog owns the lane count: 2 workers is valid (no clamping).
    let res = post_run(&app, cases_json(2, "tiny site")).await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let value: serde_json::Value =
        serde_json::from_str(&body_string(res.into_body()).await).unwrap();
    let run = store
        .get_run(value["run_id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.n_lanes, 2);
}

#[tokio::test]
async fn runs_page_and_fragment_list_runs() {
    let (app, store) = app().await;
    let run = store
        .create_run("a site about sleeping VMs", 4, 1_732_020_000_000)
        .await
        .unwrap();
    for res in [
        app.clone()
            .oneshot(Request::builder().uri("/runs").body(Body::empty()).unwrap())
            .await
            .unwrap(),
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/runs/table")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    ] {
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_string(res.into_body()).await;
        assert!(html.contains(&run.id));
        assert!(html.contains("a site about sleeping VMs"));
        assert!(html.contains("queued"));
    }
}

/// F25: the whole runs-table row links to the run page, the run id column is
/// hidden (the brief column got the space), and a lanes column shows n_lanes.
#[tokio::test]
async fn runs_table_rows_are_clickable_without_id_column_and_show_lanes() {
    let (app, store) = app().await;
    let run = store
        .create_run("a site about sleeping VMs", 4, 1_732_020_000_000)
        .await
        .unwrap();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/runs/table")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res.into_body()).await;
    // Row click affordance: the row carries the target href as data.
    assert!(
        html.contains(&format!(r#"data-run-link="/{}""#, run.id)),
        "row must be clickable: {html}"
    );
    // Lanes column present with the run's lane count.
    assert!(html.contains("<th>Lanes</th>"), "lanes column: {html}");
    assert!(html.contains(">4</td>"), "lane count rendered: {html}");
    // Brief column has the freed space: the full brief, no truncation.
    assert!(html.contains("a site about sleeping VMs"));
    // Id column hidden: no mono id link, no "Run" column header.
    assert!(
        !html.contains("<th>Run</th>"),
        "run id column header must be gone: {html}"
    );
    assert!(
        !html.contains("font-mono text-[13px] font-medium text-base-content hover:text-primary"),
        "the mono id link must be gone: {html}"
    );
}

/// The empty state is unchanged (no lanes column in the empty fragment).
#[tokio::test]
async fn runs_table_empty_state_unchanged() {
    let (app, _) = app().await;
    let res = app
        .oneshot(
            Request::builder()
                .uri("/runs/table")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let empty_html = body_string(res.into_body()).await;
    assert!(empty_html.contains("No runs yet"));
    assert!(!empty_html.contains("<th>Lanes</th>"));
    assert!(!empty_html.contains("data-run-link"));
}

#[tokio::test]
async fn gallery_renders_island_with_csp() {
    let (app, store) = app().await;
    let run = store
        .create_run("gallery test", 3, 1_732_020_000_000)
        .await
        .unwrap();
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
    assert_eq!(res.status(), StatusCode::OK);
    let csp = res
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let referrer = res.headers().get("referrer-policy").unwrap().clone();
    let html = body_string(res.into_body()).await;
    assert!(html.contains("data-run-id"));
    assert!(html.contains(&format!("data-events-url=\"/{}/events\"", run.id)));
    assert!(html.contains("/static/island.js"));
    assert!(csp.contains("frame-src 'self' https://*.sprites.app"));
    assert!(csp.contains("default-src 'self'"));
    assert_eq!(referrer, "no-referrer");
}

/// F25: the run page shows the FULL user prompt (brief) prominently at the
/// top, outside the SSE island (server-rendered, no JS required).
#[tokio::test]
async fn run_page_shows_the_full_brief() {
    let (app, store) = app().await;
    let brief = "a nine-arcade-machine museum for sleepy VMs — the FULL prompt stays visible";
    let run = store.create_run(brief, 3, 1_732_020_000_000).await.unwrap();
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
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res.into_body()).await;
    assert!(
        html.contains(brief),
        "run page must show the full brief: {html}"
    );
    assert!(html.contains("Brief"), "brief card label: {html}");
}

#[tokio::test]
async fn unknown_run_404() {
    let (app, _) = app().await;
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/nope-nope-nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn events_stream_full_state_snapshot() {
    let (app, store) = app().await;
    let run = store
        .create_run("sse test", 2, 1_732_020_000_000)
        .await
        .unwrap();
    store.insert_lane(&run.id, 0, "{}").await.unwrap();
    store.insert_lane(&run.id, 1, "{}").await.unwrap();
    store
        .start_impl(
            &run.id,
            0,
            "squadron-impl-0",
            "https://squadron-impl-0.sprites.app",
            "glm-5.3-flash",
            1_732_020_000_000,
            1_732_056_000_000,
        )
        .await
        .unwrap();
    store
        .update_lane_state(&run.id, 0, "running")
        .await
        .unwrap();
    store.add_lane_cost(&run.id, 0, 0.25).await.unwrap();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/{}/events", run.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    let body = res.into_body();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut stream = body.into_data_stream();
        stream.next().await.unwrap().unwrap()
    })
    .await
    .expect("SSE first frame timed out");
    let frame = String::from_utf8(first.to_vec()).unwrap();
    assert!(frame.starts_with("data: "));
    for key in [
        "\"run\":",
        "\"lanes\":",
        "\"ts\":",
        "\"costUsd\":0.25",
        "\"idx\":0",
    ] {
        assert!(frame.contains(key), "missing {key} in {frame}");
    }
}

/// F25: the SSE snapshot carries each lane's user-visible case prompt
/// (implPrompt) so the run page lane cards can show it.
#[tokio::test]
async fn events_snapshot_carries_lane_impl_prompt() {
    let (app, store) = app().await;
    let run = store
        .create_run("lane prompt test", 2, 1_732_020_000_000)
        .await
        .unwrap();
    store
        .insert_lane(
            &run.id,
            0,
            r#"{"label":"green","impl_prompt":"green arcade landing page for the brief"}"#,
        )
        .await
        .unwrap();
    store
        .insert_lane(&run.id, 1, r#"{"angle":"a","stack":"s","tone":"t"}"#)
        .await
        .unwrap();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/{}/events", run.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut stream = body.into_data_stream();
        stream.next().await.unwrap().unwrap()
    })
    .await
    .expect("SSE first frame timed out");
    let frame = String::from_utf8(first.to_vec()).unwrap();
    assert!(
        frame.contains(r#""implPrompt":"green arcade landing page for the brief""#),
        "case-driven lane must carry implPrompt: {frame}"
    );
    // Template-fallback lane (classic variant shape): exactly one implPrompt
    // on the page (the case-driven lane's), none invented for variant lanes.
    assert_eq!(
        frame.matches("implPrompt").count(),
        1,
        "only the case-driven lane carries implPrompt: {frame}"
    );
}

#[tokio::test]
async fn lane_detail_renders_sandboxed_iframe_and_plain_text_validator() {
    let (app, store) = app().await;
    let run = store
        .create_run("detail test", 2, 1_732_020_000_000)
        .await
        .unwrap();
    store.insert_lane(&run.id, 0, "{}").await.unwrap();
    store
        .start_impl(
            &run.id,
            0,
            "squadron-impl-0",
            "https://squadron-impl-0.sprites.app",
            "glm-5.3-flash",
            1_732_020_000_000,
            1_732_056_000_000,
        )
        .await
        .unwrap();
    store
        .finish_validation(
            &run.id,
            0,
            "pass",
            "<script>alert('xss')</script> Features: nice page, good colors.",
            8.5,
        )
        .await
        .unwrap();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/{}/lane/0", run.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res.into_body()).await;
    assert!(html.contains(r#"sandbox="allow-scripts""#));
    assert!(!html.contains("allow-same-origin"));
    assert!(!html.contains("allow-forms"));
    assert!(!html.contains("allow-top-navigation"));
    assert!(html.contains(r#"referrerpolicy="no-referrer""#));
    assert!(html.contains(r#"src="https://squadron-impl-0.sprites.app""#));
    assert!(html.contains("&lt;script&gt;"));
    assert!(!html.contains("<script>alert"));
    assert!(html.contains("Features: nice page"));
}

/// F18: the lane page renders the report arrays as lists (not raw
/// "see array" strings) and the quality score as a plain number without the
/// broken "/10" denominator.
#[tokio::test]
async fn lane_detail_renders_report_arrays_and_plain_quality_number() {
    let (app, store) = app().await;
    let run = store
        .create_run("report arrays test", 2, 1_732_020_000_000)
        .await
        .unwrap();
    store.insert_lane(&run.id, 0, "{}").await.unwrap();
    store
        .start_impl(
            &run.id,
            0,
            "squadron-impl-0",
            "https://squadron-impl-0.sprites.app",
            "glm-5.3-flash",
            1_732_020_000_000,
            1_732_056_000_000,
        )
        .await
        .unwrap();
    store
        .finish_validation(&run.id, 0, "pass", "detailed report", 71.5)
        .await
        .unwrap();
    let report = serde_json::json!({
        "verdict": "pass",
        "features": ["paddle controls", "score table"],
        "bugs": ["ball clips through wall", "see array"],
        "security": [],
        "quality_score": 71.5,
        "summary": "detailed report"
    })
    .to_string();
    store
        .set_validator_report(&run.id, 0, &report)
        .await
        .unwrap();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/{}/lane/0", run.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res.into_body()).await;
    assert!(html.contains("quality 71.50"), "html: {html}");
    assert!(!html.contains("/ 10"), "html: {html}");
    assert!(html.contains("<li>paddle controls</li>"), "html: {html}");
    assert!(html.contains("<li>score table</li>"), "html: {html}");
    assert!(
        html.contains("<li>ball clips through wall</li>"),
        "html: {html}"
    );
    assert!(
        !html.contains("see array"),
        "placeholder text must not render: {html}"
    );
    assert!(
        !html.contains("Security"),
        "empty security list renders no section: {html}"
    );
}

#[tokio::test]
async fn tokei_fragment_renders_plain_text() {
    let (app, store) = app().await;
    let run = store
        .create_run("tokei test", 2, 1_732_020_000_000)
        .await
        .unwrap();
    store.insert_lane(&run.id, 1, "{}").await.unwrap();
    let tokei = r#"{"Total":{"files":9,"code":1204}}"#;
    store
        .set_artifacts(&run.id, 1, "t.tar.gz", tokei, "log")
        .await
        .unwrap();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/{}/lane/1/tokei", run.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res.into_body()).await;
    assert!(html.contains("&quot;code&quot;:1204"));
    assert!(html.contains("<pre"));
}

#[tokio::test]
async fn thumb_gated_on_done_state() {
    let (app, store) = app().await;
    let run = store
        .create_run("thumb test", 2, 1_732_020_000_000)
        .await
        .unwrap();
    store.insert_lane(&run.id, 0, "{}").await.unwrap();
    let png = std::env::temp_dir().join(format!("squadron-test-{}.png", std::process::id()));
    std::fs::write(&png, b"\x89PNG fake bytes").unwrap();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/{}/thumb/0", run.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    store
        .set_artifacts(&run.id, 0, "t.tar.gz", "{}", "log")
        .await
        .unwrap();
    store
        .set_screenshot_state(&run.id, 0, "done", Some(png.to_str().unwrap()))
        .await
        .unwrap();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/{}/thumb/0", run.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let _ = std::fs::remove_file(&png);
}

#[tokio::test]
async fn source_download_serves_tarball_attachment() {
    let (app, store) = app().await;
    let run = store
        .create_run("tar test", 2, 1_732_020_000_000)
        .await
        .unwrap();
    store.insert_lane(&run.id, 0, "{}").await.unwrap();
    let tar = std::env::temp_dir().join(format!("squadron-test-{}.tar.gz", std::process::id()));
    std::fs::write(&tar, b"fake gzip bytes").unwrap();
    store
        .set_artifacts(&run.id, 0, tar.to_str().unwrap(), "{}", "log")
        .await
        .unwrap();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/{}/source/0", run.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let headers = res.headers();
    assert!(headers
        .get(header::CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap()
        .contains("lane-0.tar.gz"));
    let _ = std::fs::remove_file(&tar);
}

#[tokio::test]
async fn promote_requires_judged_status() {
    let (app, store) = app().await;
    let run = store
        .create_run("promote test", 2, 1_732_020_000_000)
        .await
        .unwrap();
    let token = csrf_token_for(&app, &format!("/{run_id}", run_id = run.id)).await;
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/{}/promote", run.id))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("squadron_csrf={token}"))
                .body(Body::from(format!("csrf_token={token}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    store
        .finish_run(
            &run.id,
            "judged",
            "great site",
            1,
            Some(1.0),
            1_732_020_999_000,
        )
        .await
        .unwrap();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/{}/promote", run.id))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("squadron_csrf={token}"))
                .body(Body::from(format!("csrf_token={token}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let run = store.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(run.status, "promoted");
}

/// F27: a partial run (some lanes validated, some failed) still has a winner
/// among the survivors — it is promotable exactly like a judged run.
#[tokio::test]
async fn promote_accepts_partial_status() {
    let (app, store) = app().await;
    let run = store
        .create_run("partial promote test", 2, 1_732_020_000_000)
        .await
        .unwrap();
    store
        .finish_run(
            &run.id,
            "partial",
            "winner among survivors",
            1,
            Some(1.0),
            1_732_020_999_000,
        )
        .await
        .unwrap();
    let token = csrf_token_for(&app, &format!("/{run_id}", run_id = run.id)).await;
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/{}/promote", run.id))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("squadron_csrf={token}"))
                .body(Body::from(format!("csrf_token={token}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let run = store.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(run.status, "promoted");
}

#[tokio::test]
async fn fixture_snapshot_matches_contract() {
    let (app, _) = app().await;
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/__fixtures/run")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let json = body_string(res.into_body()).await;
    for key in [
        "\"winnerLane\":1",
        "\"judgeVerdict\":",
        "\"judgeBriefFulfillment\":{\"state\":\"fulfilled\",\"evidence\":",
        "\"thumbnailDataUrl\":\"data:image/png;base64,",
        "\"costSeries\":",
        "\"consoleTail\":",
        "\"screenshotState\":\"done\"",
        "\"screenshotState\":\"pending\"",
        "\"state\":\"failed\"",
        "\"state\":\"validated\"",
        "\"valVerdict\":\"pass\"",
        "\"implUrl\":",
    ] {
        assert!(json.contains(key), "fixture missing {key}");
    }
    let lanes: Vec<serde_json::Value> = serde_json::from_str::<serde_json::Value>(&json)
        .unwrap()
        .get("lanes")
        .unwrap()
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(lanes.len(), 3);
}

#[tokio::test]
async fn fixture_gallery_and_events_served() {
    let (app, _) = app().await;
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/__fixtures/gallery")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res.into_body()).await;
    assert!(html.contains(r#"data-events-url="/__fixtures/events""#));
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/__fixtures/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut stream = body.into_data_stream();
        stream.next().await.unwrap().unwrap()
    })
    .await
    .expect("fixture SSE timed out");
    assert!(String::from_utf8(first.to_vec())
        .unwrap()
        .contains("data: "));
}

#[tokio::test]
async fn static_files_served_with_traversal_blocked() {
    let (app, _) = app().await;
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
    assert_eq!(res.headers().get(header::CONTENT_TYPE).unwrap(), "text/css");
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/static/../src/lib.rs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_json_routes_coexist_with_ui_pages() {
    let (app, store) = app().await;
    let run = store
        .create_run("api test", 3, 1_732_020_000_000)
        .await
        .unwrap();
    for uri in [
        "/api/runs".to_owned(),
        format!("/runs/{}", run.id),
        format!("/api/runs/{}", run.id),
    ] {
        let res = app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "GET {uri}");
        let body = body_string(res.into_body()).await;
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        match value {
            serde_json::Value::Array(runs) => assert_eq!(runs.len(), 1),
            serde_json::Value::Object(snap) => {
                assert_eq!(snap.get("run").unwrap().get("id").unwrap(), &run.id);
                assert!(snap.get("lanes").unwrap().as_array().is_some());
                assert!(snap.get("ts").is_some());
            }
            other => panic!("unexpected JSON shape: {other}"),
        }
    }
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_string(res.into_body()).await, "ok");
}

#[tokio::test]
async fn api_post_run_creates_run_without_html_redirect() {
    let (app, store) = app().await;
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runs")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"brief":"api created run","n_lanes":4}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = body_string(res.into_body()).await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    let run_id = value.get("run_id").unwrap().as_str().unwrap().to_owned();
    assert_eq!(value.get("n_lanes").unwrap(), &4);
    let run = store.get_run(&run_id).await.unwrap().unwrap();
    assert_eq!(run.brief, "api created run");
    assert_eq!(store.list_lanes(&run_id).await.unwrap().len(), 4);
}

/// R2-3#5: out-of-range n_lanes is a client error (400 + JSON), never a 500.
/// n_lanes=2 is valid now (spec: validation mini-swarm size).
#[tokio::test]
async fn api_post_run_rejects_bad_n_lanes_with_400_json() {
    let (app, store) = app().await;
    for n in [1, 11] {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/runs")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"brief":"api brief","n_lanes":{n}}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "n_lanes={n}");
        let value: serde_json::Value =
            serde_json::from_str(&body_string(res.into_body()).await).unwrap();
        assert!(value.get("error").is_some(), "error field expected");
    }
    assert!(store.list_runs().await.unwrap().is_empty());
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runs")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"brief":"mini swarm","n_lanes":2}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED, "n_lanes=2 must be valid");
    let value: serde_json::Value =
        serde_json::from_str(&body_string(res.into_body()).await).unwrap();
    assert_eq!(value.get("n_lanes").unwrap(), &2);
}

/// R2-3#5: briefs are capped at 4000 chars on both the API and the confirm.
#[tokio::test]
async fn api_and_confirm_reject_overlong_brief() {
    let (app, store) = app().await;
    let long = "x".repeat(4001);
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runs")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"brief":"{}"}}"#, long)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    let res = post_run(&app, cases_json(2, &long)).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // Boundary: exactly 4000 chars is accepted.
    let ok = "x".repeat(4000);
    let res = post_run(&app, cases_json(2, &ok)).await;
    assert_eq!(res.status(), StatusCode::CREATED);
    assert_eq!(store.list_runs().await.unwrap().len(), 1);
}

/// R2-3#3: malformed run ids 404 on the gallery page and the API route.
#[tokio::test]
async fn invalid_run_id_is_404_on_gallery_and_api() {
    let (app, _) = app().await;
    let bad = "z".repeat(30); // right length class of chars, wrong length
    for uri in [
        format!("/{bad}"),
        format!("/api/runs/{bad}"),
        "/runs/x".to_owned(),
    ] {
        let res = app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "GET {uri}");
    }
}

/// R2-3#2: static assets keep their max-age; everything else stays no-store.
#[tokio::test]
async fn static_assets_keep_max_age_while_pages_no_store() {
    let (app, _) = app().await;
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
    assert_eq!(
        res.headers().get(header::CACHE_CONTROL).unwrap(),
        "max-age=300"
    );
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/runs").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        res.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store, max-age=0"
    );
}
/// #26: full CSP snapshot (script-src, frame-ancestors, object-src,
/// X-Content-Type-Options) plus a negative test that raw inline script in lane
/// data is HTML-escaped, never emitted.
#[tokio::test]
async fn full_csp_snapshot_and_inline_script_escaped() {
    let (app, store) = app().await;
    let run = store
        .create_run("csp test", 3, 1_732_020_000_000)
        .await
        .unwrap();
    store.insert_lane(&run.id, 0, "{}").await.unwrap();
    store
        .set_lane_error(
            &run.id,
            0,
            "gate: <script>alert(1)</script> <img src=x onerror=alert(1)>",
        )
        .await
        .unwrap();

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/{}/lane/0", run.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let csp = res
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    for directive in [
        "default-src 'self'",
        "script-src 'self'",
        "style-src 'self'",
        "connect-src 'self'",
        "form-action 'self'",
        "base-uri 'self'",
        "object-src 'none'",
        "frame-ancestors 'self'",
    ] {
        assert!(csp.contains(directive), "CSP missing `{directive}`: {csp}");
    }
    assert_eq!(
        res.headers()
            .get(header::X_CONTENT_TYPE_OPTIONS)
            .unwrap()
            .to_str()
            .unwrap(),
        "nosniff"
    );

    let html = body_string(res.into_body()).await;
    assert!(
        !html.contains("<script>alert(1)"),
        "raw inline script must not reach the HTML"
    );
    assert!(
        html.contains("&lt;script&gt;"),
        "escaped error text expected"
    );
    assert!(
        !html.contains("<img src=x"),
        "raw img tag must not reach the HTML"
    );
}

/// Review F15: the deployed surface round — empty-state placement, sample
/// chip labels, robots.txt, HSTS, Secure CSRF cookie.
#[tokio::test]
async fn runs_page_empty_state_renders_outside_the_table() {
    let (app, _) = app().await;
    for uri in ["/runs", "/runs/table"] {
        let res = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_string(res.into_body()).await;
        assert!(html.contains("No runs yet"), "GET {uri}");
        assert!(html.contains("Launch one"), "GET {uri}");
        assert!(html.contains("squadron.fly.dev/…"), "GET {uri}");
        // No runs means no <table> at all: the empty state must not sit
        // inside the min-width table that clips it on small screens.
        assert!(!html.contains("<table"), "GET {uri}");
    }
}

#[tokio::test]
async fn runs_page_with_rows_renders_the_table() {
    let (app, store) = app().await;
    store
        .create_run("table test", 3, 1_732_020_000_000)
        .await
        .unwrap();
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/runs").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res.into_body()).await;
    assert!(html.contains("<table"));
    assert!(!html.contains("No runs yet"));
}

/// F27: the three terminal outcomes render as distinct badges — judged
/// (green), partial (amber/warning: some lanes validated, some failed), failed
/// (red) — in both the runs table and the run page header.
#[tokio::test]
async fn status_badges_render_judged_partial_and_failed() {
    let (app, store) = app().await;
    let judged = store
        .create_run("judged run", 2, 1_732_020_000_000)
        .await
        .unwrap();
    let partial = store
        .create_run("partial run", 2, 1_732_020_000_001)
        .await
        .unwrap();
    let failed = store
        .create_run("failed run", 2, 1_732_020_000_002)
        .await
        .unwrap();
    store
        .finish_run(
            &judged.id,
            "judged",
            "winner card",
            0,
            None,
            1_732_020_999_000,
        )
        .await
        .unwrap();
    store
        .finish_run(
            &partial.id,
            "partial",
            "winner among survivors",
            0,
            None,
            1_732_020_999_000,
        )
        .await
        .unwrap();
    store.update_run_status(&failed.id, "failed").await.unwrap();

    let chip = |cls: &str, label: &str| format!(r#"<span class="chip {cls}">{label}</span>"#);

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/runs/table")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html = body_string(res.into_body()).await;
    assert!(
        html.contains(&chip("chip-warning", "partial")),
        "partial badge (amber) must render in the runs table: {html}"
    );
    assert!(html.contains(&chip("chip-success", "judged")));
    assert!(html.contains(&chip("chip-error", "failed")));

    // Run page header shows the partial badge too, and the verdict card data
    // (the judge DID run — there is a winner among the survivors).
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/{}", partial.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res.into_body()).await;
    assert!(
        html.contains(&chip("chip-warning", "partial")),
        "partial badge (amber) must render on the run page: {html}"
    );
}

#[tokio::test]
async fn index_sample_chips_use_complete_short_labels() {
    let (app, _) = app().await;
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let html = body_string(res.into_body()).await;
    assert!(html.contains("1996 Netscape homepage"));
    assert!(html.contains("VM product marketing site"));
    assert!(html.contains("90s arcade clone"));
    assert!(
        !html.contains("&gt;..."),
        "chips must not be ellipsis-truncated"
    );
}

#[tokio::test]
async fn index_meta_and_mobile_nav_present() {
    let (app, _) = app().await;
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let html = body_string(res.into_body()).await;
    assert!(html.contains(r#"<meta name="description""#));
    assert!(html.contains(r#"property="og:title""#));
    assert!(html.contains(r#"name="theme-color""#));
    assert!(html.contains("one brief, N agents, one winner"));
    // The lone nav link is visible at all widths (no `hidden sm:flex`).
    assert!(!html.contains("hidden sm:flex"));
}

#[tokio::test]
async fn runs_nav_pill_has_aria_current() {
    let (app, _) = app().await;
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/runs").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let html = body_string(res.into_body()).await;
    assert!(html.contains(r#"aria-current="page""#));
}

#[tokio::test]
async fn robots_txt_served() {
    let (app, _) = app().await;
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/robots.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/plain"
    );
    let body = body_string(res.into_body()).await;
    assert!(body.contains("User-agent: *"));
    assert!(body.contains("Allow: /"));
}

#[tokio::test]
async fn hsts_and_secure_csrf_cookie_set() {
    let (app, _) = app().await;
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let hsts = res
        .headers()
        .get("strict-transport-security")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert_eq!(hsts, "max-age=31536000; includeSubDomains");
    let cookie = res
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .expect("form GET must set the squadron_csrf cookie")
        .to_owned();
    assert!(cookie.contains("Secure"), "cookie: {cookie}");
    assert!(cookie.contains("HttpOnly"), "cookie: {cookie}");
    assert!(cookie.contains("SameSite=Lax"), "cookie: {cookie}");
}

/// R2 finding 6: a stream attached to an already-terminal run sends exactly
/// one snapshot and ends — no perpetual DB polling.
#[tokio::test]
async fn sse_stream_ends_for_terminal_run() {
    let (app, store) = app().await;
    let run = store
        .create_run("terminal sse test", 2, 1_732_020_000_000)
        .await
        .unwrap();
    store
        .finish_run(
            &run.id,
            "judged",
            "verdict card",
            1,
            Some(1.0),
            1_732_020_999_000,
        )
        .await
        .unwrap();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/{}/events", run.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body();
    let frames = tokio::time::timeout(std::time::Duration::from_secs(5), async move {
        let mut stream = body.into_data_stream();
        let mut frames = Vec::new();
        while let Some(chunk) = stream.next().await {
            frames.push(chunk.unwrap());
        }
        frames
    })
    .await
    .expect("terminal SSE stream did not end");
    assert!(
        !frames.is_empty(),
        "the final snapshot is still delivered once"
    );
    assert!(frames.iter().any(|f| {
        let s = String::from_utf8_lossy(f);
        s.starts_with("data: ") && s.contains("\"status\":\"judged\"")
    }));
    assert!(
        !frames
            .iter()
            .any(|f| String::from_utf8_lossy(f).contains(": hb")),
        "no heartbeats are needed for a terminal run"
    );
}

/// R2 finding 6: the hub payload is the wake signal; when it reports a
/// terminal status the stream delivers the final snapshot and ends.
#[tokio::test]
async fn sse_stream_ends_when_hub_reports_terminal_status() {
    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    let state = AppState::new(
        store.clone(),
        SpriteClient::from_env(),
        PipelineConfig::default(),
    )
    .with_fixtures(true)
    .with_static_dir(PathBuf::from(STATIC_DIR))
    .without_pipeline();
    let run = store
        .create_run("hub sse test", 2, 1_732_020_000_000)
        .await
        .unwrap();
    store.insert_lane(&run.id, 0, "{}").await.unwrap();

    // Register a hub for the run the way start_pipeline does.
    let hub = orchestrator::SseHub::new(16);
    state.hubs.write().await.insert(run.id.clone(), hub.clone());
    let app = router(Arc::new(state));

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/{}/events", run.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body();

    // Wake the stream with a terminal hub payload.
    hub.publish(orchestrator::snapshot::Snapshot {
        run: orchestrator::snapshot::RunSnapshot {
            id: run.id.clone(),
            status: "judged".to_owned(),
            judge_verdict: Some("winner card".to_owned()),
            winner_lane: Some(0),
            judge_brief_fulfillment: None,
        },
        lanes: vec![],
        ts: 1_732_021_000_000,
    });

    let frames = tokio::time::timeout(std::time::Duration::from_secs(5), async move {
        let mut stream = body.into_data_stream();
        let mut frames = Vec::new();
        while let Some(chunk) = stream.next().await {
            frames.push(chunk.unwrap());
        }
        frames
    })
    .await
    .expect("hub-driven SSE stream did not end");
    assert!(
        frames.len() >= 2,
        "initial snapshot plus a wake-driven update expected"
    );
}
