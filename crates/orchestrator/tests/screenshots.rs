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
