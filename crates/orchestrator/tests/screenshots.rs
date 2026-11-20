use std::time::Duration;

use orchestrator::ScreenshotQueue;
use store::Store;

/// #3: the screenshot queue must actually run jobs: pending -> queued ->
/// rendering -> done, and persist the state transition in the store.
#[tokio::test(flavor = "multi_thread")]
async fn screenshot_queue_transitions_to_done() {
    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    store.create_run("brief", 3, 0).await.unwrap();
    let run = store.list_runs().await.unwrap().remove(0);
    store.insert_lane(&run.id, 0, "{}").await.unwrap();

    let queue = ScreenshotQueue::new(store.clone(), true);
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
