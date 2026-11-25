//! F30: lazy lane revival end to end. The production `revive_lane` runs the
//! SSRF-safe public probe and execs the lane's stored run_command through the
//! mock sprite CLI (which really executes `sh -c`), so this test drives the
//! whole flow: a lane whose public URL is dead (nothing listening) gets its
//! detached server restarted from the stored run_command, the public 200 is
//! re-verified, and the `lane_revived` event is logged. A second attempt
//! inside the one-per-hour window is skipped outright.
#![allow(clippy::unwrap_used, clippy::panic)] // test code: panics on failure are the assertion mechanism

use std::path::PathBuf;
use std::time::Duration;

use orchestrator::revive::{revive_lane, ReviveOutcome, EVENT_REVIVED, EVENT_REVIVE_FAILED};
use sprites_client::{SpriteClient, SpriteConfig};
use store::Store;

#[tokio::test(flavor = "multi_thread")]
async fn dead_lane_is_revived_from_the_stored_run_command_and_capped() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    store.create_run("brief", 1, 0).await.unwrap();
    let run = store.list_runs().await.unwrap().remove(0);
    store.insert_lane(&run.id, 0, "{}").await.unwrap();
    store
        .update_lane_state(&run.id, 0, "validated")
        .await
        .unwrap();

    let sprite = SpriteClient::new(SpriteConfig {
        cli_path: option_env!("CARGO_BIN_EXE_mock-sprite-cli")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mock-sprite-cli")
            }),
        command_timeout: Duration::from_secs(30),
        ..SpriteConfig::default()
    });
    sprite.create("revive-test-lane").await.unwrap();
    sprite
        .set_url_auth_public("revive-test-lane")
        .await
        .unwrap();

    // Build the site the way an implementer would: entry HTML inside a
    // dedicated serve dir; the stored run_command serves ONLY that dir.
    let workdir = dir.path().join("squadron-work/revive/lane-0");
    std::fs::create_dir_all(workdir.join("site")).unwrap();
    std::fs::write(workdir.join("site/index.html"), "<h1>revived</h1>\n").unwrap();
    // Grab a free port, then release it for the revived server to take.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let run_command = format!("cd site && python3 -m http.server {port} --bind 0.0.0.0");
    store
        .append_lane_event(&run.id, 0, store::now_ms(), "run_command", &run_command)
        .await
        .unwrap();

    let url = format!("http://127.0.0.1:{port}/");
    let outcome = revive_lane(
        &store,
        &sprite,
        &run.id,
        0,
        "revive-test-lane",
        &url,
        &workdir.to_string_lossy(),
        true, // test-only loopback escape for the SSRF-safe probe
    )
    .await;
    assert_eq!(
        outcome,
        ReviveOutcome::Attempted(true),
        "the revive must restart the detached server and see the public 200 again"
    );
    let events = store.list_lane_events(&run.id, 0).await.unwrap();
    assert!(
        events.iter().any(|e| e.kind == EVENT_REVIVED),
        "lane_revived event must be logged: {events:?}"
    );
    assert!(
        events.iter().all(|e| e.kind != EVENT_REVIVE_FAILED),
        "no failure event on the happy path: {events:?}"
    );
    let lane = store.get_lane(&run.id, 0).await.unwrap().unwrap();
    assert_eq!(lane.state, "validated", "revive must not change lane state");

    // Attempt cap: the second view within the one-per-hour window is skipped
    // before any probe or exec (the events above already carry the first
    // attempt's timestamp).
    let outcome = revive_lane(
        &store,
        &sprite,
        &run.id,
        0,
        "revive-test-lane",
        &url,
        &workdir.to_string_lossy(),
        true,
    )
    .await;
    assert_eq!(outcome, ReviveOutcome::SkippedCooldown);

    // Cleanup: the detached server outlives the mock exec on purpose; kill it.
    let _ = sprite
        .exec(
            "revive-test-lane",
            &format!("pkill -f 'http.server {port}' || true"),
            &Default::default(),
        )
        .await;

    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
    std::env::remove_var("MOCK_CREATE_MS");
    std::env::remove_var("MOCK_EXEC_MS");
}
