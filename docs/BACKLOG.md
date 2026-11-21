# Backlog

## From code review 2026-11-20 (not blocking deploy)

> Provenance note: the reviewer's original B1–B9 write-up was not included in
> the handoff materials for this fix wave (only the F1–F5 fix-now findings were
> provided in full). The paragraphs below are reconstructed follow-ups from
> that same review round, derived from the code state the review covered and
> from the F1–F5 fixes. They should be replaced/confirmed against the
> reviewer's verbatim text when it is available. Items F1–F5 themselves are
> fixed in commits 7fc25f4, c9695a7, 1306153, 31cbf1f and 76fa693.

- **B1 — screenshot state divergence window between the queue cache and the DB.**
  F1 stopped `set_artifacts` from clobbering the screenshot columns
  (crates/store/src/store.rs:332-357), but the serial queue still keeps its
  own in-memory rendered/done cache that never re-reads the DB while running:
  any state written outside the queue (manual DB edits, a second process, or a
  restart mid-render) can leave the cache and `lanes.screenshot_state` in
  disagreement, which is exactly the /thumb-404s-until-restart symptom F1
  papered over. Follow-up: make the queue treat the DB as the source of truth
  (or reconcile on each state read), and add a test for the
  restart-mid-render path (crates/orchestrator/src/screenshot.rs,
  crates/store/src/store.rs:359-370).

- **B2 — sweep every outbound fetch site through the shared SSRF machinery.**
  F2 moved the 200-oracle onto the net.rs policy
  (crates/orchestrator/src/artifacts.rs:153-176, used from
  crates/orchestrator/src/pipeline.rs:487, 979), and the judge already uses
  `fetch_public_text`. Audit the remaining ad-hoc request sites (artifact
  pull/tarball fetch, any future webhook or metadata call) so no raw
  `reqwest::get` survives; also reconsider the documented TEST-ONLY
  `FetchPolicy::allow_loopback` hatch so it cannot leak into production code
  paths (crates/orchestrator/src/net.rs:20-30, 95).

- **B3 — rate limiter is best-effort, in-memory, and proxy-blind.** The F3
  limiter keys on the socket remote addr
  (crates/swarm/src/ratelimit.rs:42-60); behind a reverse proxy every client
  shares one bucket, and a X-Forwarded-For mis-reader would make the cap
  trivially bypassable or self-DoS. The per-IP map also grows without
  eviction, and limits reset on restart. Follow-up: document the deployment
  assumption, add bounded eviction, and evaluate trusted-proxy header
  handling (crates/swarm/src/ratelimit.rs, crates/swarm/src/api.rs:62-130).

- **B4 — CSRF token lifetime and rotation.** The double-submit token is
  generated per form render and never expires or rotates on use
  (crates/swarm/src/csrf.rs, crates/swarm/src/web.rs:54-97, 371-433); a token
  leaked once stays valid indefinitely. Consider an expiry/rotation scheme and
  a test for token-reuse-after-consumption semantics
  (crates/swarm/tests/, new file).

- **B5 — 429-reject vs queue for the process-wide run cap.** F3 rejects with
  429 + Retry-After when `max_active_runs` is hit
  (crates/swarm/src/api.rs:120-130, crates/swarm/src/web.rs:133, defaults in
  crates/swarm/src/ratelimit.rs:32-34). The review noted queueing as the
  alternative: evaluate a bounded admission queue with queue-position feedback
  instead of a hard reject, and align the cap with the orchestrator's own
  slot limit (`max_running_slots`, crates/orchestrator/src/config.rs:32-47) so
  the two knobs cannot contradict.

- **B6 — Chromium sandbox fallback is silent-ish and the hatch is permanent.**
  F4 defaults the sandbox ON, retries once with the escape hatch on launch
  failure, and records the mode in lane_events
  (crates/orchestrator/src/screenshot.rs:85-140). Follow-ups: emit a
  structured warning/metric whenever the no-sandbox fallback serves a render
  (it means untrusted lane content runs unsandboxed), and track a removal plan
  for `SWARM_CHROMIUM_NO_SANDBOX=1` once the Fly image is verified to launch
  sandboxed in production.

- **B7 — the 0.0.0.0 bind rule is prompt-only; enforcement is post-hoc.** F5
  put the all-interfaces requirement into the implementer prompt, the
  fallback constraints and the curl-floor protocol
  (crates/prompts/src/roles.rs:33-40, crates/prompts/src/fallback.rs:42-49,
  crates/orchestrator/src/harness.rs:59-65), with the public 200-gate as the
  only enforcement (crates/orchestrator/src/pipeline.rs:487). An implementer
  that binds loopback still burns a full lane session before the gate catches
  it. Follow-up: cheap early check of `start-server.sh` content for
  127.0.0.1/localhost binds before the long implementer round-trip, or fail
  the lane at first turn when the detached server is detected loopback-bound.

- **B8 — document and validate the new env knobs.** The review added several
  env-configurable limits (SWARM_MAX_CONCURRENT_RUNS_PER_IP,
  SWARM_RUNS_PER_HOUR_PER_IP, SWARM_MAX_ACTIVE_RUNS, SWARM_CHROMIUM_NO_SANDBOX)
  with sane defaults but no operator-facing documentation or validation
  feedback (crates/swarm/src/ratelimit.rs:42-60,
  crates/orchestrator/src/screenshot.rs:103-108,
  crates/orchestrator/src/config.rs). Add a config/README section and
  startup-log the effective limits so a misconfigured cap is visible.

- **B9 — mock-CLI command log is unbounded and the real-mode test is
  env-hostile.** F5 added `MockState::command_log`
  (crates/sprites-client/src/mock_cli.rs:18-28, 143-175) which grows without
  bound across a long-lived mock process, and the ordering test lives in
  crates/orchestrator/tests/real_mode.rs, a single-mutation-test file that
  must stay isolated (it mutates cwd/PATH/MOCK_* env). Consider truncating the
  log or adding a ring-buffer, and keep real_mode.rs single-test discipline
  documented in the file header.
