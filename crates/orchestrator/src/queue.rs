use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::pipeline::{publish, HarnessMode, PipelineCtx};

/// Global lane-slot queue (Wave D): runs whose lanes do not all fit into the
/// free lane-slot pool at creation time wait in `queued` status and are
/// promoted strictly FIFO as slots free (a slot frees when a lane's implementer
/// or validator session ends — the permit drop sites notify the dispatcher).
///
/// The pool itself stays the existing process-wide `SlotSemaphore`; the
/// dispatcher only decides WHEN a queued run may proceed. Promotion
/// pre-reserves the run's lane permits (`try_reserve_n`) so a promoted run can
/// never be overtaken by later acquirers.
#[derive(Debug, Default)]
pub struct LaneQueue {
    inner: Mutex<VecDeque<QueuedRun>>,
    notify: tokio::sync::Notify,
}

/// One waiting run. The run's own `PipelineCtx` travels with it so promotion
/// reuses that run's SSE hub and harness cache (both are per-run).
#[derive(Debug, Clone)]
pub struct QueuedRun {
    pub ctx: Arc<PipelineCtx>,
    pub run_id: String,
    pub lanes: usize,
    pub harness: HarnessMode,
    pub decompose_cost: Option<f64>,
}

impl LaneQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a queued run and wake the dispatcher. Registration alone never
    /// promotes: the dispatcher only proceeds when the head run's lanes fit.
    pub fn enqueue(&self, run: QueuedRun) {
        self.locked().push_back(run);
        self.notify.notify_one();
    }

    /// Wake the dispatcher (called from the slot-free sites in the pipeline).
    pub fn notify_slots_free(&self) {
        self.notify.notify_one();
    }

    pub fn len(&self) -> usize {
        self.locked().len()
    }

    pub fn is_empty(&self) -> bool {
        self.locked().is_empty()
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, VecDeque<QueuedRun>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn pop(&self) -> Option<QueuedRun> {
        self.locked().pop_front()
    }
}

/// Spawn the FIFO dispatcher task for the process lifetime.
pub fn spawn_dispatcher(queue: Arc<LaneQueue>) {
    tokio::spawn(async move {
        loop {
            queue.notify.notified().await;
            promote_while_head_fits(&queue).await;
        }
    });
}

/// Promote queued runs strictly FIFO while the head run's lanes fit into the
/// free slot pool. A head that does not fit blocks the whole queue (no
/// skip-ahead) — that is what makes promotion FIFO.
async fn promote_while_head_fits(queue: &LaneQueue) {
    loop {
        let Some(head) = queue.locked().front().map(|r| {
            (
                r.ctx.clone(),
                r.run_id.clone(),
                r.lanes,
                r.harness.clone(),
                r.decompose_cost,
            )
        }) else {
            return;
        };
        let (ctx, run_id, lanes, harness, decompose_cost) = head;
        let capacity = ctx.slots.capacity();
        let available = ctx.slots.available();
        // A run that fits the pool reserves its lane permits up front so the
        // dispatcher can account exactly; an oversized run (more lanes than
        // the pool holds) may only start when the pool is completely free and
        // its lanes then acquire one at a time, exactly like pre-queue runs.
        let reservation = if lanes <= capacity {
            match ctx.slots.try_reserve_n(lanes) {
                Some(reserve) => Some(reserve),
                None => return,
            }
        } else if available != capacity {
            return;
        } else {
            None
        };
        // The head fit: take it out of the queue and run it.
        let _ = queue.pop();
        promote_run(ctx, run_id, lanes, harness, decompose_cost, reservation).await;
    }
}

/// Flip one queued run to `running` and start its pipeline task.
async fn promote_run(
    ctx: Arc<PipelineCtx>,
    run_id: String,
    lanes: usize,
    harness: HarnessMode,
    decompose_cost: Option<f64>,
    reservation: Option<Vec<tokio::sync::OwnedSemaphorePermit>>,
) {
    let _ = ctx.store.update_run_status(&run_id, "running").await;
    let _ = ctx
        .store
        .append_lane_event(
            &run_id,
            0,
            store::now_ms(),
            "log",
            &format!("run promoted from queue: {lanes} lane slots reserved"),
        )
        .await;
    publish(&ctx, &run_id).await;
    crate::pipeline::spawn_execute_run(ctx, run_id, lanes, harness, decompose_cost, reservation);
}

/// Wave D: spawn-failure classification for slot exhaustion. The platform
/// refuses sprite creation with an org-concurrency-limit error shape; such a
/// failure requeues the lane with backoff instead of failing it.
pub fn is_slot_exhaustion_error(e: &str) -> bool {
    let l = e.to_lowercase();
    let org = l.contains("org") || l.contains("organization") || l.contains("fleet");
    let limit = l.contains("limit") || l.contains("exceed") || l.contains("quota");
    org && limit
}

/// F26: sprite-create rate-limit classifier. The platform refuses a create
/// burst with "Sprite creation rate limit exceeded. You can create 10 sprites
/// per minute." — a transient condition that must requeue the lane instead of
/// failing it (same retry-not-fail semantics as slot exhaustion).
pub fn is_sprite_rate_limit_error(e: &str) -> bool {
    let l = e.to_lowercase();
    l.contains("rate limit exceeded") || l.contains("sprites per minute")
}

/// Bounded requeue budget for sprite-create rate-limit failures: the limit is
/// a rolling per-minute window, so a handful of patient retries is enough; a
/// persistent limit still fails the lane with the clear classifier error.
pub const RATE_LIMIT_REQUEUE_MAX_ATTEMPTS: usize = 3;

/// Backoff before rate-limit requeue attempt `attempt` (1-based): at least one
/// full rate-limit window (>= 60s), growing linearly so attempt 3 is well
/// clear of the window.
pub fn rate_limit_requeue_delay(attempt: usize, base: Duration) -> Duration {
    base.saturating_mul(attempt as u32)
}

/// F26: process-wide sprite-create throttle. Parallel lanes create their
/// sprites in a burst and trip the platform's 10-per-minute org rate limit;
/// spacing the create calls at most one per `spacing` (7s comfortably fits 10
/// per minute) keeps normal operation under the limit without ever seeing the
/// error.
///
/// Holding the create lock across the wait strictly serializes concurrent
/// acquirers, so the actual create calls are spaced `spacing` apart.
#[derive(Debug)]
pub struct SpriteCreateThrottle {
    spacing: Duration,
    last: tokio::sync::Mutex<Option<tokio::time::Instant>>,
}

impl SpriteCreateThrottle {
    pub fn new(spacing: Duration) -> Self {
        Self {
            spacing,
            last: tokio::sync::Mutex::new(None),
        }
    }

    /// No-spacing throttle for tests and contexts that must not wait.
    pub fn disabled() -> Self {
        Self::new(Duration::ZERO)
    }

    /// Pure spacing computation (unit-testable without sleeping): how long a
    /// create at `now` must wait given the last create time.
    pub fn wait_needed(
        last: Option<tokio::time::Instant>,
        now: tokio::time::Instant,
        spacing: Duration,
    ) -> Option<Duration> {
        let last = last?;
        Some(spacing.saturating_sub(now.saturating_duration_since(last)))
    }

    /// Serialize and space out sprite creates: callers acquire before each
    /// `sprite create`; the wait is computed from the previous create's
    /// completion time, and the stamp is taken after the wait so the NEXT
    /// acquirer is spaced from the actual create time.
    pub async fn acquire(&self) {
        let mut last = self.last.lock().await;
        if let Some(wait) = Self::wait_needed(*last, tokio::time::Instant::now(), self.spacing) {
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }
        *last = Some(tokio::time::Instant::now());
    }
}

/// Bounded requeue budget for slot-exhaustion spawn failures: the lane waits
/// out a growing backoff and retries; it only degrades to the ordinary
/// failure path after this many consecutive requeues.
pub const SLOT_REQUEUE_MAX_ATTEMPTS: usize = 50;

/// Backoff before requeue attempt `attempt` (1-based): base * 2^(n-1), capped.
pub fn slot_requeue_delay(attempt: usize, base: Duration) -> Duration {
    let exp = attempt.saturating_sub(1).min(6) as u32;
    (base.saturating_mul(1u32 << exp)).min(Duration::from_secs(60))
}

/// The planning-first cutoff: ~10% of the implementer deadline after run
/// start, never less than 3 minutes. The implementer must have written
/// `plan.md` before any server code and may keep planning until then.
pub fn plan_until_ms(started_ms: i64, deadline: Duration) -> i64 {
    let min = 3 * 60 * 1000;
    let ten_pct = deadline.as_millis() as i64 / 10;
    started_ms + ten_pct.max(min)
}

/// Human-readable form of the planning cutoff for the implementer prompt.
pub fn plan_until_label(started_ms: i64, deadline: Duration) -> String {
    let minutes = ((plan_until_ms(started_ms, deadline) - started_ms) / 60_000).max(1);
    format!("session minute {minutes} (~{minutes} minutes in)")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // test code: panics on failure are the assertion mechanism

    use super::*;

    #[test]
    fn slot_exhaustion_classifier_matches_org_limit_shapes_only() {
        assert!(is_slot_exhaustion_error(
            "create: org sprite limit of 8 reached (org-limit)"
        ));
        assert!(is_slot_exhaustion_error(
            "create: organization quota exceeded"
        ));
        assert!(is_slot_exhaustion_error(
            "create: fleet limit reached for team"
        ));
        assert!(!is_slot_exhaustion_error(
            "create: sprite name already exists"
        ));
        assert!(!is_slot_exhaustion_error("create: connection refused"));
        assert!(!is_slot_exhaustion_error("exec: temporary mock failure"));
        assert!(!is_slot_exhaustion_error("implementer: deadline exceeded"));
    }

    #[test]
    fn rate_limit_classifier_matches_the_platform_error_shape_only() {
        assert!(is_sprite_rate_limit_error(
            "sprite CLI `create foo --skip-console`: Error: Sprite creation rate limit exceeded. You can create 10 sprites per minute."
        ));
        assert!(is_sprite_rate_limit_error(
            "create: sprites per minute quota hit"
        ));
        assert!(!is_sprite_rate_limit_error(
            "create: org sprite limit of 8 reached (org-limit)"
        ));
        assert!(!is_sprite_rate_limit_error(
            "create: sprite name already exists"
        ));
        assert!(!is_sprite_rate_limit_error("create: connection refused"));
    }

    #[test]
    fn rate_limit_requeue_backoff_is_at_least_one_window() {
        let base = Duration::from_secs(60);
        assert_eq!(rate_limit_requeue_delay(1, base), Duration::from_secs(60));
        assert_eq!(rate_limit_requeue_delay(2, base), Duration::from_secs(120));
        assert_eq!(rate_limit_requeue_delay(3, base), Duration::from_secs(180));
    }

    #[test]
    fn throttle_wait_needed_computes_spacing() {
        let spacing = Duration::from_secs(7);
        let t0 = tokio::time::Instant::now();
        // first create never waits
        assert_eq!(SpriteCreateThrottle::wait_needed(None, t0, spacing), None);
        // immediately after a create: the full spacing remains
        assert_eq!(
            SpriteCreateThrottle::wait_needed(Some(t0), t0, spacing),
            Some(spacing)
        );
        // half the spacing elapsed: the remainder remains
        assert_eq!(
            SpriteCreateThrottle::wait_needed(Some(t0), t0 + Duration::from_secs(3), spacing),
            Some(Duration::from_secs(4))
        );
        // spacing fully elapsed: no wait (saturates at zero)
        assert_eq!(
            SpriteCreateThrottle::wait_needed(Some(t0), t0 + spacing, spacing),
            Some(Duration::ZERO)
        );
        assert_eq!(
            SpriteCreateThrottle::wait_needed(Some(t0), t0 + spacing * 3, spacing),
            Some(Duration::ZERO)
        );
    }

    /// Concurrent acquirers serialize with at least the configured spacing
    /// between the actual acquire times (small spacing so the test is fast).
    #[tokio::test]
    async fn throttle_serializes_concurrent_acquires_with_spacing() {
        let spacing = Duration::from_millis(20);
        let throttle = std::sync::Arc::new(SpriteCreateThrottle::new(spacing));
        let mut joins = Vec::new();
        for _ in 0..5 {
            let t = throttle.clone();
            joins.push(tokio::spawn(async move {
                t.acquire().await;
                tokio::time::Instant::now()
            }));
        }
        let mut times: Vec<tokio::time::Instant> = Vec::new();
        for j in joins {
            times.push(j.await.unwrap());
        }
        times.sort();
        // first acquire has no wait; each subsequent one is spaced
        for pair in times.windows(2) {
            let gap = pair[1].saturating_duration_since(pair[0]);
            assert!(
                gap >= spacing,
                "acquires must be spaced >= {spacing:?}, got {gap:?}"
            );
        }
        // total elapsed proves spacing held across all acquirers
        let span = times[times.len() - 1].saturating_duration_since(times[0]);
        assert!(span >= spacing * 4, "span {span:?} must cover 4 gaps");
    }

    #[test]
    fn requeue_backoff_grows_and_caps() {
        let base = Duration::from_secs(2);
        assert_eq!(slot_requeue_delay(1, base), Duration::from_secs(2));
        assert_eq!(slot_requeue_delay(2, base), Duration::from_secs(4));
        assert_eq!(slot_requeue_delay(3, base), Duration::from_secs(8));
        assert_eq!(slot_requeue_delay(7, base), Duration::from_secs(60));
        assert_eq!(slot_requeue_delay(50, base), Duration::from_secs(60));
    }

    #[test]
    fn plan_until_is_ten_percent_of_deadline_min_three_minutes() {
        // 60min deadline -> 10% = 6min
        assert_eq!(plan_until_ms(0, Duration::from_secs(3600)), 6 * 60 * 1000);
        // 20min deadline -> 2min < 3min floor -> 3min
        assert_eq!(plan_until_ms(0, Duration::from_secs(1200)), 3 * 60 * 1000);
        // 10min deadline -> 1min -> floored to 3min
        assert_eq!(
            plan_until_ms(1_000, Duration::from_secs(600)),
            1_000 + 3 * 60 * 1000
        );
        // the label is session-relative minutes
        assert_eq!(
            plan_until_label(0, Duration::from_secs(3600)),
            "session minute 6 (~6 minutes in)"
        );
        assert_eq!(
            plan_until_label(0, Duration::from_secs(600)),
            "session minute 3 (~3 minutes in)"
        );
    }
}
