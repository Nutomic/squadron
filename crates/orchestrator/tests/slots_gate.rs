mod common;

use std::time::Duration;

use common::{canned_impl, canned_val, harness, sh, wait_lanes_settled, ENV_LOCK};
use orchestrator::{start_run, RunRequest, SlotSemaphore};

// ---------------------------------------------------------------------------
// Direct SlotSemaphore tests (finding #23)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn slot_semaphore_bounds_concurrency_and_counts_acquires() {
    let slots = SlotSemaphore::new(2);
    assert_eq!(slots.available(), 2);

    let mut handles = Vec::new();
    for _ in 0..6 {
        let slots = slots.clone();
        handles.push(tokio::spawn(async move {
            let permit = slots.acquire().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(permit);
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    assert_eq!(slots.total_acquired(), 6);
    assert_eq!(slots.in_flight(), 0);
    assert!(
        slots.peak_in_flight() <= 2,
        "peak in flight must never exceed the slot count"
    );
    assert_eq!(slots.available(), 2, "all permits released");
}

#[tokio::test(flavor = "multi_thread")]
async fn slot_semaphore_queues_waiting_acquirers() {
    let slots = SlotSemaphore::new(1);
    let first = slots.acquire().await;
    assert_eq!(slots.available(), 0);
    assert_eq!(slots.in_flight(), 1);

    let slots2 = slots.clone();
    let queued = tokio::spawn(async move {
        let p = slots2.acquire().await;
        assert_eq!(slots2.in_flight(), 1, "queue, do not over-acquire");
        drop(p);
    });
    // the queued task cannot finish while `first` is held
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!queued.is_finished());
    assert_eq!(slots.total_acquired(), 1);
    drop(first);
    tokio::time::timeout(Duration::from_secs(2), queued)
        .await
        .expect("queued acquirer resumed after release")
        .unwrap();
    assert_eq!(slots.total_acquired(), 2);
    assert_eq!(slots.peak_in_flight(), 1);
}

// ---------------------------------------------------------------------------
// Pipeline-level slot exhaustion / queueing (finding #23)
// ---------------------------------------------------------------------------

/// N > slots: with 5 lanes and 2 slots, acquires must queue instead of
/// exceeding the cap, every lane must still complete, and the instrumented
/// counters must reflect exactly one acquire per phase.
#[tokio::test(flavor = "multi_thread")]
async fn more_lanes_than_slots_queues_and_never_exceeds_cap() {
    let _guard = ENV_LOCK.lock().await;
    let (ctx, store, _dir) = common::setup_ctx(|cfg| {
        cfg.max_running_slots = 2;
        cfg.n_lanes = 5;
    })
    .await;

    // each implementer holds its slot for a while so queuing is observable
    let impl_cmd = format!("sleep 0.3; {}", sh(&canned_impl()));
    let req = RunRequest {
        brief: "slot exhaustion test".to_owned(),
        n_lanes: 5,
        harness: harness(&impl_cmd, &sh(&canned_val())),
    };
    let run = start_run(ctx.clone(), req).await.unwrap();
    let lanes = wait_lanes_settled(&store, &run.id).await;

    assert_eq!(lanes.len(), 5);
    assert!(
        lanes.iter().all(|l| l.state == "validated"),
        "queued lanes must still complete: {:?}",
        lanes
            .iter()
            .map(|l| (&l.idx, &l.state, &l.error))
            .collect::<Vec<_>>()
    );

    // 5 implementer acquires + 5 validator acquires
    assert_eq!(ctx.slots.total_acquired(), 10);
    assert_eq!(ctx.slots.in_flight(), 0);
    assert!(
        ctx.slots.peak_in_flight() <= 2,
        "peak concurrent slot holders must be capped at 2"
    );
    assert!(
        ctx.slots.peak_in_flight() >= 2,
        "with 5 lanes > 2 slots the cap must actually be reached"
    );

    let final_run = store.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(final_run.status, "judged");

    common::clear_mock_env();
}

// ---------------------------------------------------------------------------
// Schema-gate rejection at the pipeline level (finding #23)
// ---------------------------------------------------------------------------

/// Invalid implementer JSON: the lane fails, no summary is stored, and the raw
/// payload never reaches the database or the SSE stream.
#[tokio::test(flavor = "multi_thread")]
async fn invalid_impl_json_fails_lane_without_storing_raw_output() {
    let _guard = ENV_LOCK.lock().await;
    let (ctx, store, _dir) = common::setup_ctx(|cfg| {
        cfg.n_lanes = 3;
    })
    .await;

    const RAW_MARKER: &str = "RAW-IMPL-PAYLOAD-{not json}";
    let impl_cmd = format!("sleep 0.1; echo '{RAW_MARKER}'");
    let req = RunRequest {
        brief: "schema gate test".to_owned(),
        n_lanes: 3,
        harness: harness(&impl_cmd, &sh(&canned_val())),
    };
    let run = start_run(ctx.clone(), req).await.unwrap();
    let lanes = wait_lanes_settled(&store, &run.id).await;

    for lane in &lanes {
        assert_eq!(lane.state, "failed");
        assert!(lane.impl_summary.is_none(), "no summary may be stored");
        assert!(lane.impl_files.is_none());
        assert!(lane.val_verdict.is_none());
        assert!(
            lane.error
                .as_deref()
                .unwrap_or_default()
                .contains("impl schema"),
            "lane error should name the schema gate: {:?}",
            lane.error
        );
    }

    // nothing raw in the DB: neither lane columns nor the event log carry the
    // un-gated payload
    for lane in &lanes {
        let err = lane.error.clone().unwrap_or_default();
        assert!(
            !err.contains(RAW_MARKER),
            "raw output leaked into lane error: {err:?}"
        );
        let events = store.list_lane_events(&run.id, lane.idx).await.unwrap();
        for ev in &events {
            assert!(
                !ev.payload.contains(RAW_MARKER),
                "raw output leaked into event log: {}",
                ev.payload
            );
        }
    }

    // nothing raw in the SSE snapshot either
    let snap = orchestrator::snapshot::build_snapshot(&store, &run.id)
        .await
        .unwrap();
    let frame = serde_json::to_string(&snap).unwrap();
    assert!(
        !frame.contains(RAW_MARKER),
        "raw output leaked into SSE: {frame}"
    );

    let final_run = store.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(final_run.status, "failed");

    common::clear_mock_env();
}
