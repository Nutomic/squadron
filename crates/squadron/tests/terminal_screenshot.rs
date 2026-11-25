#![allow(clippy::unwrap_used, clippy::panic)] // test code: panics on failure are the assertion mechanism

//! Terminal-run screenshot re-adoption (R2 backlog: "screenshot failed-state
//! re-adoption"). On a fully completed run, a lane screenshot still `pending`
//! (never attempted — the queue drained at terminal time) or `failed`
//! (attempted and settled without a frame) used to never resolve: the live
//! adoption path only enqueues not-yet-started renders, and `request` adopts
//! a persisted terminal state forever. Viewing the run now re-adopts both
//! through ONE bounded re-render attempt per lane per hour (cooldown,
//! persisted as a `screenshot_requeue` lane event), with the capture gate +
//! revive machinery deciding live/dead, and the gallery card (the snapshot
//! the island fetches) and the lane detail page reporting the same terminal
//! state.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use orchestrator::{PipelineConfig, StatusProbe};
use sprites_client::SpriteClient;
use store::Store;
use tower::ServiceExt;

use squadron::{router, AppState};

const STATIC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/static");
const MOCK_CONNECT_INFO: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 1234);

/// Deterministic counting gate probe: always 2xx (the gate passes; the render
/// itself then fails for lack of a real Chromium), counting every attempt so
/// the tests can prove a render was — or was not — attempted.
fn counting_probe() -> (StatusProbe, Arc<std::sync::atomic::AtomicUsize>) {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe_count = count.clone();
    let probe: StatusProbe = Arc::new(move |_: String| {
        probe_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok::<u16, String>(200) })
    });
    (probe, count)
}

/// App with a REAL-mode screenshot queue (test_mode off: the capture gate
/// runs) whose data dir lives in a tempdir and whose gate probe is the
/// counting fake. CHROMIUM_BIN is pinned to a nonexistent binary for the
/// duration so a re-adopted render settles to `failed` deterministically —
/// no real browser, no network — proving the state never sticks at pending.
async fn terminal_app(
    probe: StatusProbe,
) -> (
    axum::Router,
    Store,
    orchestrator::ScreenshotQueue,
    tempfile::TempDir,
) {
    let key = "CHROMIUM_BIN";
    // All tests in this binary pin the SAME bogus value, so concurrent
    // setters cannot corrupt each other; the original is restored per call.
    std::env::set_var(key, "/nonexistent-chromium-terminal-screenshot");

    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = PipelineConfig {
        data_dir: dir.path().join("data"),
        ..PipelineConfig::default()
    };
    let state = AppState::new(store.clone(), SpriteClient::from_env(), config)
        .with_static_dir(PathBuf::from(STATIC_DIR))
        .without_pipeline();
    // The queue's probe cell is shared through Arc: wiring a clone patches the
    // state's own queue (the same contract state.rs's late `with_revive` uses).
    let screenshots = state.screenshots.clone().with_probe_hook(probe);
    let app = router(Arc::new(state)).layer(axum::extract::connect_info::MockConnectInfo(
        MOCK_CONNECT_INFO,
    ));
    (app, store, screenshots, dir)
}

/// One terminal run with one hard-failed lane that has a public URL. The lane
/// state "failed" keeps the F30 lazy-revive path out of the picture (only
/// done/validated/judged lanes are revived), so the queue's injected probe is
/// the only probe in play.
async fn seed_terminal_run(store: &Store, run_status: &str, screenshot_state: &str) -> String {
    let run = store
        .create_run("terminal screenshot re-adoption", 1, 1_732_020_000_000)
        .await
        .unwrap();
    store.insert_lane(&run.id, 0, "{}").await.unwrap();
    store
        .start_impl(
            &run.id,
            0,
            "sprite-0",
            "https://lane.example/",
            "test-model",
            0,
            0,
        )
        .await
        .unwrap();
    store.update_lane_state(&run.id, 0, "failed").await.unwrap();
    store
        .set_screenshot_state(&run.id, 0, screenshot_state, None)
        .await
        .unwrap();
    store.update_run_status(&run.id, run_status).await.unwrap();
    run.id
}

fn gallery_get(run_id: &str) -> Request<Body> {
    Request::builder()
        .uri(format!("/{run_id}"))
        .body(Body::empty())
        .unwrap()
}

async fn requeue_events(store: &Store, run_id: &str) -> usize {
    store
        .list_lane_events(run_id, 0)
        .await
        .unwrap()
        .iter()
        .filter(|e| e.kind == "screenshot_requeue")
        .count()
}

/// Gallery view of a TERMINAL run whose lane screenshot settled `failed`:
/// the view re-enqueues ONE bounded render — the capture gate runs (the mock
/// probe answers) — and the state settles back to `failed` with no frame
/// (the render fails for lack of Chromium), never stuck. A second view
/// within the cooldown window must NOT re-enqueue.
#[tokio::test(flavor = "multi_thread")]
async fn gallery_view_readopts_a_failed_screenshot_with_cooldown() {
    let (probe, probes) = counting_probe();
    let (app, store, queue, _dir) = terminal_app(probe).await;
    let run_id = seed_terminal_run(&store, "judged", "failed").await;

    let res = app.clone().oneshot(gallery_get(&run_id)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // The re-adoption ran: the requeue is logged and the capture gate was hit.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if probes.load(std::sync::atomic::Ordering::SeqCst) >= 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the requeued render never reached the capture gate"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        requeue_events(&store, &run_id).await,
        1,
        "exactly one bounded re-adoption"
    );
    // The attempt settled: terminal in the worker's map AND failed in the
    // store again — with no frame stored for the failed render.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if queue.pending_state(&run_id, 0).await.as_deref() == Some("failed") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the requeued render never settled"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let lane = store.get_lane(&run_id, 0).await.unwrap().unwrap();
    assert_eq!(lane.screenshot_state.as_deref(), Some("failed"));
    assert!(lane.screenshot_path.is_none(), "no frame without a render");

    // Cooldown: the second view within the hour is a no-op — no gate probe,
    // no second requeue event.
    let res = app.clone().oneshot(gallery_get(&run_id)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        probes.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the cooldown must skip the second re-enqueue"
    );
    assert_eq!(requeue_events(&store, &run_id).await, 1);
}

/// Gallery view of a TERMINAL run whose lane screenshot is still `pending`
/// (never attempted): the same bounded adoption runs and the state settles
/// (failed here) instead of showing "screenshot pending" forever.
#[tokio::test(flavor = "multi_thread")]
async fn gallery_view_readopts_a_never_attempted_pending_screenshot() {
    let (probe, probes) = counting_probe();
    let (app, store, _queue, _dir) = terminal_app(probe).await;
    let run_id = seed_terminal_run(&store, "partial", "pending").await;

    let res = app.clone().oneshot(gallery_get(&run_id)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if probes.load(std::sync::atomic::Ordering::SeqCst) >= 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the pending lane was never adopted for a render"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // The lane settles to a terminal state (failed — the render has no
    // Chromium in this test) instead of staying pending forever.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let lane = store.get_lane(&run_id, 0).await.unwrap().unwrap();
        if lane.screenshot_state.as_deref() == Some("failed") {
            assert!(lane.screenshot_path.is_none());
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the adopted render never settled (stuck pending?)"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(requeue_events(&store, &run_id).await, 1);
}

/// A live (non-terminal) run keeps the fresh-attempt adoption only: a `failed`
/// screenshot is NOT requeued from a view (the pipeline owns that lane).
#[tokio::test(flavor = "multi_thread")]
async fn non_terminal_run_does_not_requeue_a_failed_screenshot() {
    let (probe, probes) = counting_probe();
    let (app, store, _queue, _dir) = terminal_app(probe).await;
    let run_id = seed_terminal_run(&store, "running", "failed").await;

    let res = app.clone().oneshot(gallery_get(&run_id)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        probes.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a live run's failed screenshot must not re-render from a view"
    );
    assert_eq!(requeue_events(&store, &run_id).await, 0);
}

/// Card/detail consistency: the snapshot the gallery card renders (the island
/// fetches /api/runs/{id}) and the lane detail page report the SAME terminal
/// screenshot state — the store's raw state, verbatim — and the built island
/// bundle labels a failed screenshot "failed" (never the pending fallback).
#[tokio::test(flavor = "multi_thread")]
async fn gallery_snapshot_and_lane_detail_agree_on_the_screenshot_state() {
    let (probe, _probes) = counting_probe();
    let (app, store, _queue, _dir) = terminal_app(probe).await;
    let run_id = seed_terminal_run(&store, "judged", "failed").await;

    // Detail page: the raw state, verbatim.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/{run_id}/lane/0"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(
        html.contains("Screenshot</dt><dd class=\"mono-num text-right\">failed</dd>"),
        "lane detail must show the raw screenshot state: {html}"
    );

    // Gallery card data: the snapshot carries the same state.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/runs/{run_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let json =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(
        json.contains("\"screenshotState\":\"failed\""),
        "the gallery snapshot must carry the store's screenshot state: {json}"
    );

    // The card's label logic ships in the built island bundle: a failed
    // screenshot reads "screenshot failed" (matching the detail page), never
    // the pending fallback.
    let bundle =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/static/island.js")).unwrap();
    assert!(
        bundle.contains("screenshot failed"),
        "the card must label failed screenshots as failed"
    );
}
