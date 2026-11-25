#![allow(clippy::unwrap_used, clippy::panic)] // test code: panics on failure are the assertion mechanism

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use orchestrator::{HubRegistry, ScreenshotQueue};
use store::Store;
use tokio::sync::RwLock;

/// The queue's data dir must come from a tempdir, never the /data fallback:
/// `ScreenshotQueue::new` resolves DATA_DIR (or /data), which fails on a
/// non-root dev box (audit R3 finding 4).
fn queue_data_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    (dir, data_dir)
}

/// #3: the screenshot queue must actually run jobs: pending -> queued ->
/// rendering -> done, and persist the state transition in the store.
#[tokio::test(flavor = "multi_thread")]
async fn screenshot_queue_transitions_to_done() {
    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    store.create_run("brief", 3, 0).await.unwrap();
    let run = store.list_runs().await.unwrap().remove(0);
    store.insert_lane(&run.id, 0, "{}").await.unwrap();

    let (_dir, data_dir) = queue_data_dir();
    let queue = ScreenshotQueue::with_data_dir(store.clone(), true, data_dir);
    let state = queue.request(&run.id, 0, "https://example.invalid").await;
    assert_eq!(state, "queued", "first request enqueues");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let lane = store.get_lane(&run.id, 0).await.unwrap().unwrap();
        let s = lane.screenshot_state.clone().unwrap_or_default();
        if s == "done" {
            assert!(
                lane.screenshot_path.is_some(),
                "rendered screenshot path persisted"
            );
            break;
        }
        if matches!(s.as_str(), "rendering" | "queued" | "pending" | "") {
            if tokio::time::Instant::now() > deadline {
                panic!("screenshot never reached done, last state={s}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        panic!("unexpected screenshot state {s}");
    }

    // Idempotent: a second request on a done lane does not re-enqueue.
    let again = queue.request(&run.id, 0, "https://example.invalid").await;
    assert_eq!(again, "done");
}

/// R2 finding 2: ensure_rendered enqueues the job when needed and returns only
/// once a terminal state is reached (bounded internally by the caller's wait).
#[tokio::test(flavor = "multi_thread")]
async fn ensure_rendered_waits_until_terminal_state() {
    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    store.create_run("brief", 3, 0).await.unwrap();
    let run = store.list_runs().await.unwrap().remove(0);
    store.insert_lane(&run.id, 0, "{}").await.unwrap();

    let (_dir, data_dir) = queue_data_dir();
    let queue = ScreenshotQueue::with_data_dir(store.clone(), true, data_dir);
    let state = queue
        .ensure_rendered(
            &run.id,
            0,
            "https://example.invalid",
            Duration::from_secs(10),
        )
        .await;
    assert_eq!(state, "done", "test-mode render completes quickly");

    // The cache holds the terminal entry until it is evicted.
    assert_eq!(
        queue.pending_state(&run.id, 0).await,
        Some("done".to_owned())
    );

    // R2 finding 6: evict_terminal drops cached terminal entries for the run;
    // request() re-adopts the state from the store afterwards.
    queue.evict_terminal(&run.id).await;
    assert_eq!(queue.pending_state(&run.id, 0).await, None);
    let again = queue.request(&run.id, 0, "https://example.invalid").await;
    assert_eq!(again, "done", "state is re-adopted from the store");
    assert_eq!(
        queue.pending_state(&run.id, 0).await,
        Some("done".to_owned())
    );
}

/// Audit R3 finding 3: with the hub registry wired in, a screenshot state
/// change is broadcast as a snapshot to the run's SSE hub, so a live gallery
/// subscriber sees the thumbnail without a reload.
#[tokio::test(flavor = "multi_thread")]
async fn screenshot_state_changes_reach_the_sse_hub() {
    use orchestrator::SseHub;

    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    store.create_run("brief", 3, 0).await.unwrap();
    let run = store.list_runs().await.unwrap().remove(0);
    store.insert_lane(&run.id, 0, "{}").await.unwrap();

    let registry: HubRegistry = Arc::new(RwLock::new(std::collections::HashMap::new()));
    let hub = SseHub::new(64);
    registry.write().await.insert(run.id.clone(), hub.clone());
    let mut rx = hub.subscribe();

    let (_dir, data_dir) = queue_data_dir();
    let queue =
        ScreenshotQueue::with_data_dir_hubs(store.clone(), true, data_dir, Some(registry.clone()));
    queue.request(&run.id, 0, "https://example.invalid").await;

    // A snapshot carrying the terminal screenshot state must arrive.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let snap = match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(snap)) => snap,
            _ => panic!("no snapshot with a done screenshot state reached the hub"),
        };
        let done = snap
            .lanes
            .iter()
            .any(|l| l.idx == 0 && l.screenshot_state.as_deref() == Some("done"));
        if done {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("screenshot state never reached the SSE hub");
        }
    }

    // Without a registered hub the queue degrades gracefully (no panic).
    let (_dir2, data_dir2) = queue_data_dir();
    let queue_no_hubs = ScreenshotQueue::with_data_dir(store.clone(), true, data_dir2);
    let state = queue_no_hubs
        .ensure_rendered(
            &run.id,
            0,
            "https://example.invalid",
            Duration::from_secs(10),
        )
        .await;
    assert_eq!(state, "done");
}

/// Terminal-run re-adoption (R2 backlog): `requeue` re-renders a lane whose
/// persisted state is `failed` — `request` adopts the persisted terminal
/// state and would return it forever. Test mode settles the re-render to
/// `done`, proving the job actually ran again.
#[tokio::test(flavor = "multi_thread")]
async fn requeue_re_renders_a_failed_lane() {
    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    store.create_run("brief", 3, 0).await.unwrap();
    let run = store.list_runs().await.unwrap().remove(0);
    store.insert_lane(&run.id, 0, "{}").await.unwrap();

    let (_dir, data_dir) = queue_data_dir();
    let queue = ScreenshotQueue::with_data_dir(store.clone(), true, data_dir);

    // A previous queue instance settled the render to `failed` (e.g. a dead
    // lane URL at judge time): the persisted terminal state.
    store
        .set_screenshot_state(&run.id, 0, "failed", None)
        .await
        .unwrap();
    let adopted = queue.request(&run.id, 0, "https://example.invalid").await;
    assert_eq!(
        adopted, "failed",
        "request adopts the persisted failure without re-rendering"
    );

    // The terminal-run view path instead requeues: the job runs again and
    // settles to a fresh terminal state.
    let state = queue.requeue(&run.id, 0, "https://example.invalid").await;
    assert_eq!(state, "queued", "requeue re-enqueues a failed lane");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let lane = store.get_lane(&run.id, 0).await.unwrap().unwrap();
        if lane.screenshot_state.as_deref() == Some("done") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "requeued render never settled"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The requeue double-enqueue guard: an attempt already in flight
/// (queued/rendering in the live map) is returned as-is, never enqueued twice
/// — a gallery view and a lane view racing on the same lane share the queue.
#[tokio::test(flavor = "multi_thread")]
async fn requeue_never_double_enqueues_an_in_flight_job() {
    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    store.create_run("brief", 3, 0).await.unwrap();
    let run = store.list_runs().await.unwrap().remove(0);
    store.insert_lane(&run.id, 0, "{}").await.unwrap();

    let (_dir, data_dir) = queue_data_dir();
    // Real mode so the capture gate runs; the probe blocks inside the gate
    // until the test opens it, freezing the job mid-flight (rendering).
    let (gate, gate_rx) = tokio::sync::watch::channel(false);
    let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe_count = probes.clone();
    let probe_rx = gate_rx.clone();
    let probe: orchestrator::StatusProbe = Arc::new(move |_: String| {
        probe_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut rx = probe_rx.clone();
        Box::pin(async move {
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    return Err("capture gate closed".to_owned());
                }
            }
            Ok::<u16, String>(200)
        })
    });
    let queue =
        ScreenshotQueue::with_data_dir(store.clone(), false, data_dir).with_probe_hook(probe);

    assert_eq!(
        queue.request(&run.id, 0, "https://example.invalid").await,
        "queued"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if queue.pending_state(&run.id, 0).await.as_deref() == Some("rendering")
            && probes.load(std::sync::atomic::Ordering::SeqCst) == 1
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "job never reached the blocked capture gate"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // A concurrent requeue sees the in-flight job and does NOT send a second
    // one (no second probe, no duplicate render).
    let state = queue.requeue(&run.id, 0, "https://example.invalid").await;
    assert_eq!(state, "rendering");
    assert_eq!(
        probes.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "no second job may run the capture gate"
    );

    // Release the gate: the render then fails for lack of a real Chromium and
    // the job settles, leaving exactly one attempt in the evidence.
    gate.send(true).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let lane = store.get_lane(&run.id, 0).await.unwrap().unwrap();
        if lane.screenshot_state.as_deref() == Some("failed") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "in-flight job never settled"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        probes.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "exactly one capture-gate attempt total"
    );
}

/// F14 regression: in real (non-test) mode the destination directory is
/// created BEFORE the render attempt, so a render can never fail with ENOENT
/// because `DATA_DIR/screenshots/{run}` was missing. The browser is mocked by
/// pointing CHROMIUM_BIN at a nonexistent binary: the launch fails (expected),
/// but the destination dir must already exist — the old code created no dirs,
/// so this proves the mkdir happens ahead of the render.
#[tokio::test(flavor = "multi_thread")]
async fn real_mode_creates_dest_dir_before_render_attempt() {
    let key = "CHROMIUM_BIN";
    let prev = std::env::var(key).ok();
    std::env::set_var(key, "/nonexistent/chromium-for-f14-test");

    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    store.create_run("brief", 1, 0).await.unwrap();
    let run = store.list_runs().await.unwrap().remove(0);
    store.insert_lane(&run.id, 0, "{}").await.unwrap();

    let (_dir, data_dir) = queue_data_dir(); // does NOT pre-create screenshots/
                                             // F37: the capture gate probes the public URL before any render; wire a
                                             // deterministic 200 probe so this test reaches the render (with the bogus
                                             // chromium) and still proves the F14 mkdir-before-render fix.
    let probe: orchestrator::StatusProbe =
        Arc::new(|_: String| Box::pin(async { Ok::<u16, String>(200) }));
    let queue = ScreenshotQueue::with_data_dir(store.clone(), false, data_dir.clone())
        .with_probe_hook(probe);
    let state = queue
        .ensure_rendered(
            &run.id,
            0,
            "https://example.invalid",
            Duration::from_secs(15),
        )
        .await;
    assert_eq!(
        state, "failed",
        "bogus chromium binary must fail the render"
    );

    let dest = orchestrator::render_dest(&data_dir, &run.id, 0);
    assert!(dest.starts_with(&data_dir), "dest is under DATA_DIR");
    assert!(
        dest.parent().unwrap().is_dir(),
        "DATA_DIR/screenshots/<run> was created before the render attempt"
    );

    match prev {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}
