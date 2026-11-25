#![allow(clippy::unwrap_used)] // test code: panics on failure are the assertion mechanism

use store::{is_valid_run_id, new_run_id, Store, StoreError};

#[tokio::test]
async fn run_lifecycle_persists_and_lists() {
    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    let run = store
        .create_run("build a game", 3, 1_700_000_000_000)
        .await
        .unwrap();
    assert!(is_valid_run_id(&run.id));
    assert_eq!(run.status, "queued");
    assert_eq!(run.n_lanes, 3);

    let fetched = store.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(fetched, run);
    assert_eq!(store.list_runs().await.unwrap().len(), 1);
    assert!(store.get_run("missing").await.unwrap().is_none());

    store.update_run_status(&run.id, "running").await.unwrap();
    store
        .finish_run(
            &run.id,
            "judged",
            "Bold. Playable.",
            2,
            Some(1.25),
            1_700_000_100_000,
        )
        .await
        .unwrap();
    let fetched = store.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(fetched.status, "judged");
    assert_eq!(fetched.judge_verdict.as_deref(), Some("Bold. Playable."));
    assert_eq!(fetched.winner_lane, Some(2));
    assert_eq!(fetched.total_cost_usd, Some(1.25));
    assert_eq!(fetched.finished_at, Some(1_700_000_100_000));
    // R3-A #5: an unknown total persists NULL and reads back None.
    store
        .finish_run(
            &run.id,
            "judged",
            "Bold. Playable.",
            2,
            None,
            1_700_000_100_000,
        )
        .await
        .unwrap();
    let fetched = store.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(fetched.total_cost_usd, None);
    assert!(matches!(
        store.update_run_status("missing", "running").await,
        Err(StoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn lanes_store_all_pipeline_fields() {
    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    let run = store.create_run("brief", 2, 100).await.unwrap();
    let lane = store
        .insert_lane(&run.id, 0, "{\"angle\":\"retro\"}")
        .await
        .unwrap();
    assert_eq!(lane.state, "queued");
    assert_eq!(lane.screenshot_state.as_deref(), Some("pending"));

    store
        .update_lane_state(&run.id, 0, "running")
        .await
        .unwrap();
    store
        .start_impl(
            &run.id,
            0,
            "squadron-abc-impl-0",
            "https://squadron-abc-impl-0.sprites.app",
            "glm-5.3-flash",
            200,
            200 + 3_600_000,
        )
        .await
        .unwrap();
    store
        .finish_impl(&run.id, 0, "built a game", 7)
        .await
        .unwrap();
    store
        .start_validation(&run.id, 0, "deepseek-v4.1-flash", 300, 300 + 1_800_000)
        .await
        .unwrap();
    store
        .finish_validation(&run.id, 0, "pass", "detailed report", 91.5)
        .await
        .unwrap();
    store
        .set_artifacts(
            &run.id,
            0,
            "/data/lane0.tar.gz",
            "{\"files\":7,\"loc\":321}",
            "last console line",
        )
        .await
        .unwrap();
    store
        .set_screenshot_state(&run.id, 0, "done", Some("/data/lane0.png"))
        .await
        .unwrap();
    store.add_lane_cost(&run.id, 0, 0.1).await.unwrap();
    store.add_lane_cost(&run.id, 0, 0.2).await.unwrap();

    let lane = store.get_lane(&run.id, 0).await.unwrap().unwrap();
    assert_eq!(lane.state, "running");
    assert_eq!(lane.revisions, 0);
    store.set_lane_revisions(&run.id, 0, 2).await.unwrap();
    store.increment_lane_revisions(&run.id, 0).await.unwrap();
    assert_eq!(
        store.get_lane(&run.id, 0).await.unwrap().unwrap().revisions,
        3
    );
    assert_eq!(lane.sprite_name.as_deref(), Some("squadron-abc-impl-0"));
    assert_eq!(lane.impl_model.as_deref(), Some("glm-5.3-flash"));
    assert_eq!(lane.impl_files, Some(7));
    assert_eq!(lane.impl_deadline, Some(200 + 3_600_000));
    assert_eq!(lane.val_verdict.as_deref(), Some("pass"));
    assert_eq!(lane.val_quality, Some(91.5));
    assert_eq!(lane.screenshot_state.as_deref(), Some("done"));
    assert_eq!(lane.console_tail.as_deref(), Some("last console line"));
    assert_eq!(lane.cost_usd, Some(0.30000000000000004));

    assert_eq!(store.list_lanes(&run.id).await.unwrap().len(), 1);
    store.set_lane_error(&run.id, 0, "timeout").await.unwrap();
    assert_eq!(
        store
            .get_lane(&run.id, 0)
            .await
            .unwrap()
            .unwrap()
            .error
            .as_deref(),
        Some("timeout")
    );
    assert!(matches!(
        store.update_lane_state(&run.id, 9, "failed").await,
        Err(StoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn lane_events_are_monotonic_and_auditable() {
    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    let run = store.create_run("brief", 1, 100).await.unwrap();
    store.insert_lane(&run.id, 0, "{}").await.unwrap();

    let ts1 = store
        .append_lane_event(&run.id, 0, 500, "state", "running")
        .await
        .unwrap();
    let ts2 = store
        .append_lane_event(&run.id, 0, 500, "log", "started")
        .await
        .unwrap();
    assert!(ts2 > ts1);
    let events = store.list_lane_events(&run.id, 0).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind, "state");
    assert_eq!(events[1].payload, "started");
    assert_eq!(events[1].lane_idx, 0);
}

/// Review F1 regression: `set_artifacts` at implementer completion must not
/// overwrite a screenshot the serial queue already rendered+persisted during
/// the long impl session — screenshot_path/state stay untouched in the DB.
#[tokio::test]
async fn set_artifacts_never_clobbers_screenshot_state() {
    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    let run = store.create_run("brief", 1, 100).await.unwrap();
    store.insert_lane(&run.id, 0, "{}").await.unwrap();

    // The queue rendered and persisted a screenshot while impl was running.
    store
        .set_screenshot_state(&run.id, 0, "done", Some("/data/run/lane-0.png"))
        .await
        .unwrap();

    // Implementer completion writes its artifacts with no screenshot info.
    store
        .set_artifacts(&run.id, 0, "/data/run/lane0.tar.gz", "{}", "tail")
        .await
        .unwrap();

    let lane = store.get_lane(&run.id, 0).await.unwrap().unwrap();
    assert_eq!(lane.screenshot_state.as_deref(), Some("done"));
    assert_eq!(
        lane.screenshot_path.as_deref(),
        Some("/data/run/lane-0.png")
    );
    // The artifact columns did land.
    assert_eq!(lane.tarball_path.as_deref(), Some("/data/run/lane0.tar.gz"));
    assert_eq!(lane.console_tail.as_deref(), Some("tail"));
}

#[test]
fn generated_ids_are_distinct() {
    assert_ne!(new_run_id(), new_run_id());
}

/// Wave A: the judge's brief_fulfillment verdict round-trips through the runs
/// table, and reads back as None for runs that never had one set.
#[tokio::test]
async fn judge_brief_fulfillment_round_trips() {
    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    let run = store.create_run("brief", 1, 100).await.unwrap();

    // No fulfillment recorded yet.
    assert_eq!(
        store.get_judge_brief_fulfillment(&run.id).await.unwrap(),
        None
    );

    let json = r#"{"state":"fulfilled","evidence":"winner's site extract shows the requested arcade loop"}"#;
    store
        .set_judge_brief_fulfillment(&run.id, json)
        .await
        .unwrap();
    assert_eq!(
        store.get_judge_brief_fulfillment(&run.id).await.unwrap(),
        Some(json.to_owned())
    );

    // Unknown run id: no row -> None (not an error).
    assert_eq!(
        store
            .get_judge_brief_fulfillment("does-not-exist")
            .await
            .unwrap(),
        None
    );
}

/// Wave B: decomposition decides the lane count after run creation;
/// set_run_n_lanes persists it and rejects unknown ids.
#[tokio::test]
async fn set_run_n_lanes_updates_the_run_row() {
    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    let run = store.create_run("brief", 3, 100).await.unwrap();
    store.set_run_n_lanes(&run.id, 6).await.unwrap();
    let updated = store.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(updated.n_lanes, 6);
    assert!(store.set_run_n_lanes("missing-run", 6).await.is_err());
}
