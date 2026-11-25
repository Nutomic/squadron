//! Shared test-only helpers for the orchestrator integration binaries.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code: panics on failure are the assertion mechanism

use std::time::Duration;

use orchestrator::HubRegistry;

/// Wall-clock ceiling (seconds) for "wait until the run finishes" loops.
///
/// Generous by default: the full suite runs 20+ test binaries in parallel on
/// an 8-core box, and a healthy mock-driven run can burn minutes of wall time
/// under that load (a 60s ceiling produced false failures at ~294s binary
/// runtime). This only raises the ceiling — it adds no sleeps, so a run that
/// finishes in seconds still finishes in seconds. Override with
/// `SWARM_TEST_WAIT_SECS` for a tighter local loop.
pub fn wait_secs() -> u64 {
    std::env::var("SWARM_TEST_WAIT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(600)
}

/// Migrated store on a UNIQUE file-backed DB under `dir` (each test owns a
/// tempdir). `Store::connect_in_memory` pools a single `sqlite::memory:`
/// connection; sqlx's idle reaper (600s default) closes it when a loaded run
/// starves the test for that long, and the re-opened connection finds an
/// EMPTY in-memory database — later queries panic with `no such table: runs`
/// (the dialog.rs failure at ~601s binary runtime). A file-backed DB survives
/// connection recycling, and sqlx's default 5s busy_timeout absorbs sqlite
/// contention between the parallel test binaries.
#[allow(dead_code)] // used by the wave suites only
pub async fn migrated_store_in(dir: &std::path::Path) -> store::Store {
    let db = dir.join("squadron-test.db");
    let store = store::Store::connect(&format!("sqlite://{}?mode=rwc", db.display()))
        .await
        .expect("connect file-backed test store");
    store
        .migrate()
        .await
        .expect("migrate file-backed test store");
    store
}

/// Bounded, event-driven wait for the hub registry to drain a terminal run's
/// channel (audit R3 finding 5).
///
/// `drop_hub` runs inside the spawned pipeline task AFTER the run's DB status
/// flips terminal, so on a loaded runner the test can observe `judged`/`failed`
/// while the cleanup step has not been scheduled yet (the CI drain-race: the
/// real-mode suite failed only on 2-core GitHub runners). The fast path never
/// sleeps; polls every ~250ms only while the channel is still registered, and
/// fails only if it never drains.
#[allow(dead_code)] // used by the real_mode/smoke suites only
pub async fn wait_hub_drained(registry: &HubRegistry, run_id: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs());
    loop {
        if !registry.read().await.contains_key(run_id) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "hub registry never drained the terminal run's channel: {run_id}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
