//! Shared test-only helpers for the orchestrator integration binaries.

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
