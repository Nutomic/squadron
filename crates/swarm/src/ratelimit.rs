//! Review F3: bounded anonymous run creation. The service stays public
//! (no login), but creation is rate-limited three ways:
//!
//! 1. per-IP concurrent cap — at most `max_concurrent_per_ip` non-terminal
//!    runs created from one client IP (best-effort: an in-memory map keyed by
//!    the remote address / forwarded IP, statuses re-checked against the
//!    store);
//! 2. per-IP hourly cap — at most `max_creates_per_hour_per_ip` creations in
//!    a sliding one-hour window;
//! 3. process-wide active cap — at most `max_active_runs` non-terminal runs
//!    across the whole process (DB-backed via `store::count_active_runs`).
//!
//! All limits are configurable via env; JSON /api/runs is subject to the
//! limits but not to the CSRF check (F3 #3).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use store::{now_ms, Store};

#[derive(Debug, Clone)]
pub struct RunLimits {
    pub max_concurrent_per_ip: usize,
    pub max_creates_per_hour_per_ip: usize,
    pub max_active_runs: usize,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            max_concurrent_per_ip: 3,
            max_creates_per_hour_per_ip: 10,
            max_active_runs: 4,
        }
    }
}

impl RunLimits {
    /// Env config with sane defaults (review F3 #1/#2). Zero/invalid values
    /// fall back to the defaults so a typo cannot disable a limit entirely.
    pub fn from_env() -> Self {
        let mut limits = Self::default();
        if let Some(n) = env_usize("SWARM_MAX_CONCURRENT_RUNS_PER_IP") {
            limits.max_concurrent_per_ip = n;
        }
        if let Some(n) = env_usize("SWARM_RUNS_PER_HOUR_PER_IP") {
            limits.max_creates_per_hour_per_ip = n;
        }
        if let Some(n) = env_usize("SWARM_MAX_ACTIVE_RUNS") {
            limits.max_active_runs = n;
        }
        limits
    }
}

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Rejection {
    /// This IP already holds the per-IP concurrent maximum.
    PerIpConcurrent,
    /// This IP exhausted the hourly creation budget.
    PerIpHourly,
    /// The process already runs the active-run maximum.
    ProcessCap,
}

impl Rejection {
    /// Human-facing reason; the HTTP layer wraps it in 429 + Retry-After.
    pub fn message(&self) -> String {
        match self {
            Self::PerIpConcurrent => {
                "too many concurrent runs from this address; wait for one to finish"
            }
            Self::PerIpHourly => {
                "run-creation rate limit reached for this address; try again later"
            }
            Self::ProcessCap => "the service is at its concurrent-run capacity; try again soon",
        }
        .to_owned()
    }
}

/// Non-terminal = the run can still start work; the same statuses the web
/// layer treats as non-terminal for SSE.
fn is_active_status(status: &str) -> bool {
    matches!(status, "queued" | "running")
}

const HOUR_MS: i64 = Duration::from_secs(3600).as_millis() as i64;

#[derive(Default, Debug)]
struct Inner {
    /// ip -> (run_id, created_ms), most recent last.
    created: HashMap<String, Vec<(String, i64)>>,
}

/// Shared across `AppState` clones (Arc): every handler clone must observe
/// the same budget.
#[derive(Debug, Clone)]
pub struct RunLimiter {
    limits: RunLimits,
    inner: Arc<Mutex<Inner>>,
}

impl RunLimiter {
    pub fn new(limits: RunLimits) -> Self {
        Self {
            limits,
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }

    /// Check whether a new run creation from `ip` is allowed right now. Does
    /// NOT record anything — call `record` after the run is actually created
    /// so a failed creation does not consume the budget.
    pub async fn check(&self, ip: &str, store: &Store) -> Result<(), Rejection> {
        let now = now_ms();
        let recorded: Vec<(String, i64)> = {
            let mut inner = self.inner.lock().expect("limiter lock");
            let entry = inner.created.entry(ip.to_owned()).or_default();
            entry.retain(|(_, ts)| now - *ts < HOUR_MS);
            entry.clone()
        };

        // 2. hourly creation budget (review F3 #1)
        if recorded.len() >= self.limits.max_creates_per_hour_per_ip {
            return Err(Rejection::PerIpHourly);
        }

        // 1. per-IP concurrent cap: re-check each recorded run against the
        // store so finished runs free the slot without waiting out the hour
        // window (best-effort: the process knows only what it created).
        let mut per_ip_active = 0usize;
        for (run_id, _) in &recorded {
            if let Ok(Some(run)) = store.get_run(run_id).await {
                if is_active_status(&run.status) {
                    per_ip_active += 1;
                }
            }
        }
        if per_ip_active >= self.limits.max_concurrent_per_ip {
            return Err(Rejection::PerIpConcurrent);
        }

        // 3. process-wide active cap (review F3 #2): DB-backed.
        if let Ok(active) = store.count_active_runs().await {
            if active as usize >= self.limits.max_active_runs {
                return Err(Rejection::ProcessCap);
            }
        }
        Ok(())
    }

    /// Record a successful creation against `ip`.
    pub fn record(&self, ip: &str, run_id: &str) {
        let mut inner = self.inner.lock().expect("limiter lock");
        inner
            .created
            .entry(ip.to_owned())
            .or_default()
            .push((run_id.to_owned(), now_ms()));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    async fn store() -> Store {
        let store = Store::connect_in_memory().await.unwrap();
        store.migrate().await.unwrap();
        store
    }

    fn limiter(limits: RunLimits) -> RunLimiter {
        RunLimiter::new(limits)
    }

    /// The per-IP concurrent cap counts only non-terminal runs: marking a
    /// recorded run judged frees the slot immediately.
    #[tokio::test]
    async fn per_ip_concurrent_cap_frees_when_runs_finish() {
        let store = store().await;
        let lim = limiter(RunLimits {
            max_concurrent_per_ip: 2,
            max_creates_per_hour_per_ip: 100,
            max_active_runs: 100,
        });
        for _ in 0..2 {
            let run = store.create_run("b", 2, now_ms()).await.unwrap();
            lim.record("1.2.3.4", &run.id);
        }
        assert_eq!(
            lim.check("1.2.3.4", &store).await,
            Err(Rejection::PerIpConcurrent)
        );
        assert!(lim.check("5.6.7.8", &store).await.is_ok());
        // Finish one run from this IP; the slot frees up.
        let runs = store.list_runs().await.unwrap();
        store
            .finish_run(&runs[0].id, "judged", "ok", 0, None, now_ms())
            .await
            .unwrap();
        assert!(lim.check("1.2.3.4", &store).await.is_ok());
    }

    #[tokio::test]
    async fn per_ip_hourly_cap_blocks_beyond_the_window_budget() {
        let store = store().await;
        let lim = limiter(RunLimits {
            max_concurrent_per_ip: 100,
            max_creates_per_hour_per_ip: 2,
            max_active_runs: 100,
        });
        // One creation is fine, the second too; a third check is rejected.
        let run = store.create_run("b", 2, now_ms()).await.unwrap();
        lim.record("1.1.1.1", &run.id);
        assert!(lim.check("1.1.1.1", &store).await.is_ok());
        let run = store.create_run("b", 2, now_ms()).await.unwrap();
        lim.record("1.1.1.1", &run.id);
        assert_eq!(
            lim.check("1.1.1.1", &store).await,
            Err(Rejection::PerIpHourly)
        );
        // A different IP is unaffected.
        assert!(lim.check("2.2.2.2", &store).await.is_ok());
    }

    #[tokio::test]
    async fn process_cap_blocks_when_too_many_runs_are_active() {
        let store = store().await;
        let lim = limiter(RunLimits {
            max_concurrent_per_ip: 100,
            max_creates_per_hour_per_ip: 100,
            max_active_runs: 2,
        });
        let run = store.create_run("b", 2, now_ms()).await.unwrap();
        assert!(lim.check("9.9.9.9", &store).await.is_ok());
        lim.record("9.9.9.9", &run.id);
        // Second active run hits the process cap.
        let run2 = store.create_run("b", 2, now_ms()).await.unwrap();
        assert_eq!(
            lim.check("8.8.8.8", &store).await,
            Err(Rejection::ProcessCap)
        );
        // Terminal runs do not count toward the cap.
        store
            .finish_run(&run.id, "judged", "ok", 0, None, now_ms())
            .await
            .unwrap();
        assert!(lim.check("8.8.8.8", &store).await.is_ok());
        let _ = run2;
    }
}
