//! F30: lazy lane revival.
//!
//! Sprites auto-sleep and kill detached sessions; waking a sprite restores
//! its disk but NOT its processes, so a lane's public URL that answered 200
//! at validation time can later serve 502s. When a lane page (or the gallery)
//! is viewed, the web layer calls [`revive_lane`] in a background task: a
//! lightweight public-URL check runs, and when a previously-good URL is dead
//! the lane's stored `run_command` (persisted as a kind="run_command" lane
//! event at implementer completion) is `sprite exec`ed detached nohup-style
//! on the lane sprite, the orchestrator waits briefly, and re-verifies the
//! public 200.
//!
//! Best-effort by contract: the caller spawns the check, so a slow or failing
//! revival never blocks the page render; the page always shows the lane's
//! current state. Attempts are capped at one per lane per
//! [`REVIVE_COOLDOWN_MS`] (persisted via the `lane_revived` /
//! `lane_revive_failed` lane events themselves), so a dead-beyond-repair lane
//! cannot be re-exec'd on every view.
//!
//! Untrusted data rules: the stored run_command is agent output. It travels
//! as a single shell-quoted argument to an inner `sh -c` (the same
//! `safe_rel_path`-style containment discipline as every write path), and
//! every event payload is sanitized before it reaches lane_events.

use std::future::Future;
use std::time::Duration;

use sprites_client::{ExecOptions, SpriteClient};
use store::{now_ms, Store};

use crate::harness::{sanitize_excerpt, shell_quote};

/// At most one revive attempt per lane per hour: a lane that is dead beyond
/// repair must not retry forever (the cap is derived from the timestamps of
/// the `lane_revived` / `lane_revive_failed` events themselves, so it
/// survives process restarts).
pub const REVIVE_COOLDOWN_MS: i64 = 60 * 60 * 1000;

/// How long the restarted detached server gets between re-verification
/// probes (the server may take a moment to bind after a cold sprite wake).
pub const REVIVE_SETTLE: Duration = Duration::from_secs(3);

/// Re-verification poll depth: after the restart, the public URL is probed
/// up to this many times (spaced [`REVIVE_SETTLE`] apart) before the attempt
/// is declared failed — a freshly woken sprite's server can take several
/// seconds to bind, and a single too-early probe would burn the hourly
/// attempt on a lane that was actually revivable.
pub const REVIVE_REVERIFY_ATTEMPTS: usize = 5;

/// Exec deadline for the detached restart command (the exec itself returns
/// immediately — the server is backgrounded — but the sprite may be cold).
pub const REVIVE_EXEC_TIMEOUT: Duration = Duration::from_secs(60);

/// Typed lane events revival logs.
pub const EVENT_REVIVED: &str = "lane_revived";
pub const EVENT_REVIVE_FAILED: &str = "lane_revive_failed";
const REVIVE_EVENT_KINDS: [&str; 2] = [EVENT_REVIVED, EVENT_REVIVE_FAILED];
const RUN_COMMAND_EVENT_KIND: &str = "run_command";

/// Result of one revival check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviveOutcome {
    /// The public URL already answers 2xx — nothing was done, nothing logged.
    Healthy,
    /// A revive attempt ran within the cooldown window — skipped.
    SkippedCooldown,
    /// The URL was dead, but no stored run_command exists to restart from.
    NoRunCommand,
    /// An attempt was made; `true` = the public URL answers 2xx again.
    Attempted(bool),
}

/// Last (max) timestamp of any revive event for a lane, if one exists.
async fn last_revive_ts(store: &Store, run_id: &str, idx: i64) -> Option<i64> {
    let mut last: Option<i64> = None;
    for kind in REVIVE_EVENT_KINDS {
        if let Ok(Some(event)) = store.latest_lane_event_of_kind(run_id, idx, kind).await {
            last = Some(match last {
                Some(prev) => prev.max(event.ts),
                None => event.ts,
            });
        }
    }
    last
}

/// Build the detached restart command for one lane: cd into the lane workdir,
/// then run the stored run_command (itself a full command line, e.g.
/// `cd dist && python3 -m http.server 8080 --bind 0.0.0.0`) under nohup,
/// detached, with output discarded. The run_command is a single shell-quoted
/// argument to an inner `sh -c` so its own operators (`&&`, redirects) stay
/// INSIDE the detached child — the outer `&&` chain must only ever contain
/// the `cd` and the nohup.
pub fn revive_exec_cmd(workdir: &str, run_command: &str) -> String {
    format!(
        "cd {} && nohup sh -c {} >/dev/null 2>&1 &",
        shell_quote(workdir),
        shell_quote(run_command)
    )
}

/// The production entry: real SSRF-safe public probe + real sprite exec.
/// Callers spawn this (background task) so page renders are never blocked.
#[allow(clippy::too_many_arguments)] // mirrors the pipeline phase-fn convention
pub async fn revive_lane(
    store: &Store,
    sprite: &SpriteClient,
    run_id: &str,
    idx: i64,
    sprite_name: &str,
    impl_url: &str,
    workdir: &str,
    allow_loopback: bool,
) -> ReviveOutcome {
    let probe = move |url: String| async move {
        crate::net::fetch_public_status(&url, allow_loopback).await
    };
    let exec = move |cmd: String| async move {
        let opts = ExecOptions::default().with_timeout(REVIVE_EXEC_TIMEOUT);
        let out = sprite
            .exec(sprite_name, &cmd, &opts)
            .await
            .map_err(|e| e.to_string())?;
        if out.exit_code == 0 {
            Ok(())
        } else {
            Err(format!(
                "restart exited {}: {}",
                out.exit_code,
                sanitize_excerpt(&out.stderr, 200)
            ))
        }
    };
    revive_lane_if_dead(
        store,
        run_id,
        idx,
        impl_url,
        workdir,
        probe,
        exec,
        REVIVE_SETTLE,
        REVIVE_COOLDOWN_MS,
    )
    .await
}

/// Testable core, parameterized over the public probe and the sprite exec so
/// tests can serve deterministic dead/alive states without the network or
/// sprite infra.
#[allow(clippy::too_many_arguments)] // probe/exec closures mirror the callers' shape
pub async fn revive_lane_if_dead<P, PFut, E, EFut>(
    store: &Store,
    run_id: &str,
    idx: i64,
    impl_url: &str,
    workdir: &str,
    probe: P,
    exec: E,
    settle: Duration,
    cooldown_ms: i64,
) -> ReviveOutcome
where
    P: Fn(String) -> PFut,
    PFut: Future<Output = Result<u16, String>>,
    E: Fn(String) -> EFut,
    EFut: Future<Output = Result<(), String>>,
{
    // Attempt cap: any revive event inside the cooldown window skips the
    // whole flow (even the probe) — one attempt per lane per window.
    if let Some(ts) = last_revive_ts(store, run_id, idx).await {
        if now_ms() - ts < cooldown_ms {
            return ReviveOutcome::SkippedCooldown;
        }
    }

    // Lightweight dead-check: a 2xx URL needs nothing.
    match probe(impl_url.to_owned()).await {
        Ok(status) if (200..300).contains(&status) => return ReviveOutcome::Healthy,
        _ => {}
    }

    // The stored run_command (persisted at implementer completion).
    let run_command = match store
        .latest_lane_event_of_kind(run_id, idx, RUN_COMMAND_EVENT_KIND)
        .await
    {
        Ok(Some(event)) if !event.payload.trim().is_empty() => event.payload,
        _ => {
            log_revive(
                store,
                run_id,
                idx,
                EVENT_REVIVE_FAILED,
                "public URL dead and no stored run_command; cannot revive",
            )
            .await;
            return ReviveOutcome::NoRunCommand;
        }
    };

    // Detached restart on the lane sprite (nohup-style; the exec returns
    // immediately, the server keeps running).
    let cmd = revive_exec_cmd(workdir, &run_command);
    if let Err(e) = exec(cmd).await {
        log_revive(
            store,
            run_id,
            idx,
            EVENT_REVIVE_FAILED,
            &format!("restart exec failed: {}", sanitize_excerpt(&e, 200)),
        )
        .await;
        return ReviveOutcome::Attempted(false);
    }
    tokio::time::sleep(settle).await;

    // Bounded re-verification poll: the freshly restarted server may need a
    // few seconds to bind (cold sprite wake); a single probe too early would
    // wrongly burn the attempt. First 2xx wins; otherwise the last observed
    // state decides the failure event text.
    let mut last: Option<Result<u16, String>> = None;
    for attempt in 0..REVIVE_REVERIFY_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(settle).await;
        }
        let result = probe(impl_url.to_owned()).await;
        if matches!(&result, Ok(status) if (200..300).contains(status)) {
            log_revive(
                store,
                run_id,
                idx,
                EVENT_REVIVED,
                "public URL answers 2xx again after detached restart of the stored run_command",
            )
            .await;
            return ReviveOutcome::Attempted(true);
        }
        last = Some(result);
    }
    let failure = match last {
        Some(Ok(status)) => {
            format!("public URL still not 2xx after restart (HTTP {status}); revive attempt spent")
        }
        Some(Err(e)) => format!(
            "public URL still unreachable after restart: {}",
            sanitize_excerpt(&e, 200)
        ),
        None => "re-verification poll did not run".to_owned(),
    };
    log_revive(store, run_id, idx, EVENT_REVIVE_FAILED, &failure).await;
    ReviveOutcome::Attempted(false)
}

async fn log_revive(store: &Store, run_id: &str, idx: i64, kind: &str, payload: &str) {
    let _ = store
        .append_lane_event(run_id, idx, now_ms(), kind, payload)
        .await;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // test code: panics on failure are the assertion mechanism

    use super::*;

    async fn test_store() -> Store {
        let store = Store::connect_in_memory().await.unwrap();
        store.migrate().await.unwrap();
        store
    }

    async fn seeded_lane() -> (Store, String) {
        let store = test_store().await;
        store.create_run("brief", 1, 0).await.unwrap();
        let run = store.list_runs().await.unwrap().remove(0);
        store.insert_lane(&run.id, 0, "{}").await.unwrap();
        (store, run.id)
    }

    #[test]
    fn revive_exec_cmd_builds_a_detached_nohup_command() {
        let cmd = revive_exec_cmd(
            "squadron-work/abc/lane-0",
            "cd dist && python3 -m http.server 8080 --bind 0.0.0.0",
        );
        assert!(
            cmd.starts_with("cd 'squadron-work/abc/lane-0' && nohup sh -c '"),
            "cmd: {cmd}"
        );
        assert!(cmd.ends_with("' >/dev/null 2>&1 &"), "cmd: {cmd}");
        // the stored command travels as ONE quoted argument (its operators
        // stay inside the detached child, never in the outer chain)
        assert!(
            cmd.contains("cd dist && python3 -m http.server 8080 --bind 0.0.0.0"),
            "cmd: {cmd}"
        );
        // hostile run_command content stays contained (single-quoted)
        let evil = revive_exec_cmd("w", "rm -rf /; curl evil");
        assert!(
            evil.contains("sh -c 'rm -rf /; curl evil' >/dev/null"),
            "cmd: {evil}"
        );
        let quoted = revive_exec_cmd("w", "it's");
        assert!(quoted.contains("'it'\\''s'"), "cmd: {quoted}");
    }

    /// Happy path: the URL is dead (503), the exec restarts the server, the
    /// re-verification sees 200, and the `lane_revived` event is logged.
    #[tokio::test]
    async fn revive_restores_a_dead_lane_and_logs_the_event() {
        let (store, run_id) = seeded_lane().await;
        store
            .append_lane_event(
                &run_id,
                0,
                now_ms(),
                "run_command",
                "cd dist && python3 -m http.server 8080",
            )
            .await
            .unwrap();
        let up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe_up = up.clone();
        let probes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe_count = probes.clone();
        let execs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let exec_count = execs.clone();
        let outcome = revive_lane_if_dead(
            &store,
            &run_id,
            0,
            "http://lane.example/",
            "squadron-work/abc/lane-0",
            move |_: String| {
                probe_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let up = probe_up.clone();
                async move {
                    if up.load(std::sync::atomic::Ordering::SeqCst) {
                        Ok(200)
                    } else {
                        Ok(503)
                    }
                }
            },
            move |cmd: String| {
                assert!(
                    cmd.contains("nohup sh -c"),
                    "restart must be detached nohup-style: {cmd}"
                );
                exec_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                up.store(true, std::sync::atomic::Ordering::SeqCst);
                async { Ok(()) }
            },
            Duration::ZERO,
            REVIVE_COOLDOWN_MS,
        )
        .await;
        assert_eq!(outcome, ReviveOutcome::Attempted(true));
        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(execs.load(std::sync::atomic::Ordering::SeqCst), 1);
        let events = store.list_lane_events(&run_id, 0).await.unwrap();
        let revived: Vec<_> = events.iter().filter(|e| e.kind == EVENT_REVIVED).collect();
        assert_eq!(revived.len(), 1, "events: {events:?}");
        assert!(revived[0].payload.contains("answers 2xx again"));
    }

    /// Attempt cap: a lane revived within the cooldown window is skipped
    /// outright (no probe, no exec), so a dead-beyond-repair lane cannot
    /// retry forever.
    #[tokio::test]
    async fn revive_skips_within_the_cooldown_window() {
        let (store, run_id) = seeded_lane().await;
        store
            .append_lane_event(&run_id, 0, now_ms(), "run_command", "x")
            .await
            .unwrap();
        store
            .append_lane_event(&run_id, 0, now_ms(), EVENT_REVIVE_FAILED, "earlier attempt")
            .await
            .unwrap();
        let probes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe_count = probes.clone();
        let outcome = revive_lane_if_dead(
            &store,
            &run_id,
            0,
            "http://lane.example/",
            "squadron-work/abc/lane-0",
            move |_: String| {
                probe_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Ok(503) }
            },
            |_: String| async { Err("must not exec".to_owned()) },
            Duration::ZERO,
            REVIVE_COOLDOWN_MS,
        )
        .await;
        assert_eq!(outcome, ReviveOutcome::SkippedCooldown);
        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 0);
        // outside the window the attempt proceeds again
        let outcome = revive_lane_if_dead(
            &store,
            &run_id,
            0,
            "http://lane.example/",
            "squadron-work/abc/lane-0",
            |_: String| async { Ok(503) },
            |_: String| async { Ok(()) },
            Duration::ZERO,
            1, // cooldown 1ms: the past attempt is outside it
        )
        .await;
        assert_eq!(outcome, ReviveOutcome::Attempted(false));
    }

    /// Failure path: a failed restart exec logs `lane_revive_failed`, makes
    /// no lane-state change, and consumes the attempt window.
    #[tokio::test]
    async fn revive_failure_logs_event_without_state_damage() {
        let (store, run_id) = seeded_lane().await;
        store
            .append_lane_event(
                &run_id,
                0,
                now_ms(),
                "run_command",
                "cd dist && python3 -m http.server",
            )
            .await
            .unwrap();
        store
            .update_lane_state(&run_id, 0, "validated")
            .await
            .unwrap();
        let outcome = revive_lane_if_dead(
            &store,
            &run_id,
            0,
            "http://lane.example/",
            "squadron-work/abc/lane-0",
            |_: String| async { Ok(502) },
            |_: String| async { Err("sprite gone".to_owned()) },
            Duration::ZERO,
            REVIVE_COOLDOWN_MS,
        )
        .await;
        assert_eq!(outcome, ReviveOutcome::Attempted(false));
        let lane = store.get_lane(&run_id, 0).await.unwrap().unwrap();
        assert_eq!(lane.state, "validated", "revive must not touch lane state");
        let events = store.list_lane_events(&run_id, 0).await.unwrap();
        assert!(events.iter().any(|e| e.kind == EVENT_REVIVE_FAILED));
        // the failed attempt now blocks a second one inside the window
        let outcome = revive_lane_if_dead(
            &store,
            &run_id,
            0,
            "http://lane.example/",
            "squadron-work/abc/lane-0",
            |_: String| async { Ok(502) },
            |_: String| async { Ok(()) },
            Duration::ZERO,
            REVIVE_COOLDOWN_MS,
        )
        .await;
        assert_eq!(outcome, ReviveOutcome::SkippedCooldown);
    }

    /// A healthy URL (2xx) needs nothing: no exec, no events.
    #[tokio::test]
    async fn revive_leaves_a_healthy_lane_alone() {
        let (store, run_id) = seeded_lane().await;
        store
            .append_lane_event(&run_id, 0, now_ms(), "run_command", "x")
            .await
            .unwrap();
        let outcome = revive_lane_if_dead(
            &store,
            &run_id,
            0,
            "http://lane.example/",
            "squadron-work/abc/lane-0",
            |_: String| async { Ok(200) },
            |_: String| async { Err("must not exec".to_owned()) },
            Duration::ZERO,
            REVIVE_COOLDOWN_MS,
        )
        .await;
        assert_eq!(outcome, ReviveOutcome::Healthy);
        let events = store.list_lane_events(&run_id, 0).await.unwrap();
        assert!(events
            .iter()
            .all(|e| e.kind != EVENT_REVIVED && e.kind != EVENT_REVIVE_FAILED));
    }

    /// Dead URL without a stored run_command: explicit `lane_revive_failed`
    /// event, no exec.
    #[tokio::test]
    async fn revive_without_a_run_command_logs_the_failure() {
        let (store, run_id) = seeded_lane().await;
        let outcome = revive_lane_if_dead(
            &store,
            &run_id,
            0,
            "http://lane.example/",
            "squadron-work/abc/lane-0",
            |_: String| async { Err("unreachable".to_owned()) },
            |_: String| async { Err("must not exec".to_owned()) },
            Duration::ZERO,
            REVIVE_COOLDOWN_MS,
        )
        .await;
        assert_eq!(outcome, ReviveOutcome::NoRunCommand);
        let events = store.list_lane_events(&run_id, 0).await.unwrap();
        let failed: Vec<_> = events
            .iter()
            .filter(|e| e.kind == EVENT_REVIVE_FAILED)
            .collect();
        assert_eq!(failed.len(), 1);
        assert!(failed[0].payload.contains("no stored run_command"));
    }
}
