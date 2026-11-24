//! Shared test-only helpers for the orchestrator integration binaries.

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
