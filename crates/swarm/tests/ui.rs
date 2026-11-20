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

use swarm::{router, AppState};

const STATIC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/static");

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
    let app = router(Arc::new(state));
    (app, store)
}

async fn body_string(body: Body) -> String {
    String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

async fn post_run(app: &axum::Router, brief: &str, n: Option<&str>) -> axum::http::Response<Body> {
    let form = match n {
        Some(n) => format!("brief={}&n={}", urlencode(brief), n),
        None => format!("brief={}", urlencode(brief)),
    };
    let req = Request::builder()
        .method("POST")
        .uri("/runs")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[tokio::test]
async fn index_renders_form_chips_and_n_selector() {
    let (app, _) = app().await;
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res.into_body()).await;
    assert!(html.contains(r#"name="brief""#));
    assert!(html.contains(r#"name="n""#));
    assert!(html.contains("Netscape-era"));
    assert!(html.contains("arcade"));
    assert!(html.contains("VM product"));
    assert!(html.contains(r#"value="8" selected"#));
    assert!(html.contains(r#"value="3""#));
    assert!(html.contains(r#"value="10""#));
    assert!(!html.contains(r#"value="11""#));
}

#[tokio::test]
async fn submit_creates_run_redirects_and_creates_lanes() {
    let (app, store) = app().await;
    let res = post_run(&app, "Build a retro terminal portfolio", Some("5")).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let location = res
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(location.len() > 10);
    let run_id = location.trim_start_matches('/');
    let run = store.get_run(run_id).await.unwrap().unwrap();
    assert_eq!(run.status, "queued");
    assert_eq!(run.n_lanes, 5);
    let lanes = store.list_lanes(run_id).await.unwrap();
    assert_eq!(lanes.len(), 5);
    assert!(lanes.iter().all(|l| l.state == "queued"));
}

#[tokio::test]
async fn submit_rejects_empty_brief_and_clamps_n() {
    let (app, store) = app().await;
    let res = post_run(&app, "   ", None).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = post_run(&app, "tiny site", Some("2")).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let location = res
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let run = store
        .get_run(location.trim_start_matches('/'))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.n_lanes, 3);
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
            "swarm-impl-0",
            "https://swarm-impl-0.sprites.app",
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
            "swarm-impl-0",
            "https://swarm-impl-0.sprites.app",
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
    assert!(html.contains(r#"src="https://swarm-impl-0.sprites.app""#));
    assert!(html.contains("&lt;script&gt;"));
    assert!(!html.contains("<script>alert"));
    assert!(html.contains("Features: nice page"));
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
        .set_artifacts(&run.id, 1, "t.tar.gz", tokei, None, "pending", "log")
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
    let png = std::env::temp_dir().join(format!("swarm-test-{}.png", std::process::id()));
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
        .set_artifacts(
            &run.id,
            0,
            "t.tar.gz",
            "{}",
            Some(png.to_str().unwrap()),
            "done",
            "log",
        )
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
    let tar = std::env::temp_dir().join(format!("swarm-test-{}.tar.gz", std::process::id()));
    std::fs::write(&tar, b"fake gzip bytes").unwrap();
    store
        .set_artifacts(
            &run.id,
            0,
            tar.to_str().unwrap(),
            "{}",
            None,
            "pending",
            "log",
        )
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
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/{}/promote", run.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    store
        .finish_run(&run.id, "judged", "great site", 1, 1.0, 1_732_020_999_000)
        .await
        .unwrap();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/{}/promote", run.id))
                .body(Body::empty())
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

// ---------------------------------------------------------------------------
// Finding #26: full CSP snapshot + escaping of agent-controlled content
// ---------------------------------------------------------------------------

#[tokio::test]
async fn security_headers_carry_the_complete_csp() {
    let (app, store) = app().await;
    let run = store
        .create_run("csp snapshot", 3, 1_732_020_000_000)
        .await
        .unwrap();
    store.insert_lane(&run.id, 0, "{}").await.unwrap();
    for uri in [format!("/{}/lane/0", run.id), format!("/{}", run.id)] {
        let res = app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "uri={uri}");
        let csp = res
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        // snapshot the complete policy string; any drift fails the test
        assert_eq!(
            csp,
            "default-src 'self'; frame-src 'self' https://*.sprites.app; \
             img-src 'self' data: https://*.sprites.app; script-src 'self'; \
             style-src 'self'; connect-src 'self'; form-action 'self'; \
             base-uri 'self'; object-src 'none'; frame-ancestors 'self'",
            "uri={uri}"
        );
        assert!(
            csp.contains("script-src 'self'")
                && csp.contains("frame-ancestors 'self'")
                && csp.contains("object-src 'none'"),
            "uri={uri}"
        );
        assert_eq!(
            res.headers()
                .get(header::X_CONTENT_TYPE_OPTIONS)
                .unwrap()
                .to_str()
                .unwrap(),
            "nosniff",
            "uri={uri}"
        );
    }
}

#[tokio::test]
async fn agent_supplied_inline_script_stays_escaped_in_templates() {
    let (app, store) = app().await;
    let run = store
        .create_run("escaping test", 2, 1_732_020_000_000)
        .await
        .unwrap();
    store.insert_lane(&run.id, 0, "{}").await.unwrap();
    // agent-controlled fields carry inline script payloads
    store
        .set_lane_error(&run.id, 0, "<script>alert('pwned')</script> crashed")
        .await
        .unwrap();
    store
        .set_lane_console_tail(&run.id, 0, "<script>document.write('tail')</script> ok")
        .await
        .unwrap();
    store
        .finish_validation(
            &run.id,
            0,
            "pass",
            "<script>alert('validator')</script> Features: clean build.",
            7.0,
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
    // every payload must be entity-escaped, never executable inline script
    assert!(html.contains("&lt;script&gt;alert"));
    assert!(html.contains("&lt;script&gt;document"));
    assert!(!html.contains("<script>alert"));
    assert!(!html.contains("<script>document"));
}

// ---------------------------------------------------------------------------
// Finding #31: the canonical 22-char run-id validator guards every route
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_run_ids_are_rejected_on_all_routes() {
    let (app, store) = app().await;
    let run = store
        .create_run("id validation", 3, 1_732_020_000_000)
        .await
        .unwrap();
    let too_short: String = std::iter::repeat('A').take(21).collect();
    let too_long: String = std::iter::repeat('A').take(23).collect();
    let bad_alphabet = "!!!!!!!!!!!!!!!!!!!!!!"; // 22 chars, not URL-safe base64
    assert_eq!(bad_alphabet.len(), 22);
    for bad in [too_short, too_long, bad_alphabet.to_owned()] {
        // GET routes (read-only)
        for uri in [
            format!("/{bad}/events"),
            format!("/{bad}/lane/0"),
            format!("/{bad}/lane/0/tokei"),
            format!("/{bad}/thumb/0"),
            format!("/{bad}/source/0"),
        ] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "id={bad} uri={uri}");
        }
        // POST route
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
        assert_eq!(
            res.status(),
            StatusCode::NOT_FOUND,
            "id={bad} uri=/{bad}/promote"
        );
    }
    // sanity: the real id still works
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
}
