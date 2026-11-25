//! Shared test-only helpers for the squadron integration binaries.

use store::Store;

/// Wall-clock ceiling (seconds) for pipeline wait loops.
///
/// Same contract as the orchestrator suite's `wait_secs` (F19/F20): generous
/// by default because the full suite runs 20+ test binaries in parallel on an
/// 8-core box, and a healthy mock-driven run can burn minutes of wall time
/// under that load. This only raises the ceiling — it adds no sleeps, so a
/// run that finishes in seconds still finishes in seconds. Override with
/// `SWARM_TEST_WAIT_SECS` for a tighter local loop.
#[allow(dead_code)] // used by the wave suites only
pub fn wait_secs() -> u64 {
    std::env::var("SWARM_TEST_WAIT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(600)
}

/// Migrated store on a UNIQUE file-backed DB in a fresh tempdir.
///
/// F34: `Store::connect_in_memory` pools a single `sqlite::memory:` connection;
/// sqlx's idle reaper (600s default) closes it when a loaded run starves the
/// test for that long, and the re-opened connection finds an EMPTY in-memory
/// database — later queries panic with `no such table: runs` (the dialog.rs
/// failure at ~601s binary runtime). A file-backed DB survives connection
/// recycling, and sqlx's default 5s busy_timeout absorbs sqlite contention
/// between the parallel test binaries. The tempdir must be kept alive as long
/// as the store (the caller owns it).
#[allow(dead_code)] // used by the wave suites only
pub async fn migrated_store() -> (Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("test tempdir");
    let db = dir.path().join("squadron-test.db");
    let store = Store::connect(&format!("sqlite://{}?mode=rwc", db.display()))
        .await
        .expect("connect file-backed test store");
    store
        .migrate()
        .await
        .expect("migrate file-backed test store");
    (store, dir)
}
