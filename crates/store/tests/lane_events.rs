use std::sync::Arc;

use store::Store;

/// Finding #19: `append_lane_event` allocates ts atomically, so concurrent
/// writers on the same lane can never collide on the
/// (run_id, lane_idx, ts) primary key and timestamps stay strictly
/// increasing.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writers_allocate_unique_monotonic_ts() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}/events.db?mode=rwc", dir.path().display());
    let store = Arc::new(Store::connect_with_migrations(&url).await.unwrap());
    let run = store.create_run("brief", 1, 0).await.unwrap();
    store.insert_lane(&run.id, 0, "{}").await.unwrap();

    const WRITERS: usize = 8;
    const PER_WRITER: usize = 6;

    let mut handles = Vec::new();
    for w in 0..WRITERS {
        let store = store.clone();
        let run_id = run.id.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..PER_WRITER {
                let ts = store
                    .append_lane_event(&run_id, 0, 1_000, "log", &format!("w{w}e{i}"))
                    .await
                    .unwrap_or_else(|e| panic!("writer {w} event {i} failed: {e}"));
                assert!(ts >= 1_000, "ts floor violated: {ts}");
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let events = store.list_lane_events(&run.id, 0).await.unwrap();
    assert_eq!(events.len(), WRITERS * PER_WRITER);
    // strictly increasing: unique and ordered, no gaps back to an older ts
    for pair in events.windows(2) {
        assert!(
            pair[1].ts > pair[0].ts,
            "ts must be strictly increasing: {} then {}",
            pair[0].ts,
            pair[1].ts
        );
    }
}

/// The supplied ts acts as a floor: a later wall clock still wins.
#[tokio::test]
async fn supplied_ts_floor_is_respected() {
    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    let run = store.create_run("brief", 1, 0).await.unwrap();
    store.insert_lane(&run.id, 0, "{}").await.unwrap();

    let ts1 = store
        .append_lane_event(&run.id, 0, 100, "log", "first")
        .await
        .unwrap();
    let ts2 = store
        .append_lane_event(&run.id, 0, 100, "log", "second")
        .await
        .unwrap();
    assert_eq!(ts1, 100);
    assert!(ts2 > ts1);
    let ts3 = store
        .append_lane_event(&run.id, 0, ts2 + 5_000, "log", "later clock")
        .await
        .unwrap();
    assert_eq!(ts3, ts2 + 5_000);
}
