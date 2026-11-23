# Squadron

Squadron is a multi-agent web-builder arena: submit **one** brief for a web
project, and N AI agents each build a complete site in parallel, each in its
own [Fly.io Sprite](https://sprites.dev). Paired validators review every lane,
a judge agent compares the surviving candidates (screenshots included) and
crowns a winner, and a live gallery dashboard — server-rendered Askama +
htmx with a Svelte island, streaming over SSE — tracks the whole run from
brief to verdict. The control plane is Rust: axum HTTP server, SQLx/SQLite
persistence, a lane state machine and slot-limited pipeline, and a prompt
schema gate.

## Architecture

Six-crate Cargo workspace plus the binary crate that ties it together:

| Crate | Role |
| --- | --- |
| `squadron` | axum app: gallery dashboard (Askama templates, htmx, Svelte island), REST/SSE API, fixtures, wiring, entrypoint |
| `orchestrator` | lane state machine, slot semaphore (max running lanes), pipeline (impl → validate → judge), screenshots, artifacts, harness, judge, SSE snapshots |
| `prompts` | role prompts (implementer/validator/judge/variants), serde output types, JSON Schema documents, schema gate + injection scan |
| `sprites-client` | typed wrapper over the `sprite` CLI (create/exec/info/config), mock CLI + fixtures for tests |
| `llm-gateway` | machine LLM client (judge/variants) via `OPENCODE_KEY`, and the no-auth opencode gateway recipe builder |
| `store` | SQLx/SQLite store: runs, lanes, lane events, costs, validator reports; offline prepared-query cache |

Data flow per run: `POST /runs` (brief + N) → each lane gets a fresh Sprite
(`squadron-<run>-impl-<i>`), the implementer agent builds and serves the site on
its public `*.sprites.app` URL, a 200-check verifies it, two validators
review and report, then the judge (real LLM with `OPENCODE_KEY`, or the
deterministic fallback) renders the lanes and picks a winner. Every state
change is persisted and pushed to the dashboard via SSE.

## Quickstart

Requirements: Rust 1.75+ (edition 2021), `sqlite3` available via sqlx (bundled),
and the `sprite` CLI on `PATH` for real runs.

Environment variables:

| Variable | Meaning | Default |
| --- | --- | --- |
| `OPENCODE_KEY` | API key for the machine judge/variants client. Without it the judge falls back to a deterministic stub (dev/test only). | unset |
| `DATABASE_URL` | SQLite URL (also accepts `SQUADRON_DB_URL`). | `sqlite:/data/squadron.db?mode=rwc` |
| `DATA_DIR` | Work dir for per-lane artifacts, screenshots, tarballs. | `/data` |
| `BIND` | Listen address (`host:port`). `PORT` is used instead when `BIND` is unset. | `0.0.0.0:3000` |
| `SQUADRON_STATIC_DIR` | Directory with compiled static assets (JS/CSS). | embedded `static/` |
| `SPRITE_CLI` | Path to the `sprite` CLI binary. | `sprite` on `PATH` |
| `SPRITE_TOKEN` | Sprite org token (secret — set via `fly secrets`, never in files). | unset |
| `SQUADRON_TEST_MODE` | `1` = skip real network checks/probes (offline smoke mode). | unset |
| `SQUADRON_FIXTURES` | `1` = serve canned fixture runs under `/__fixtures/*` for UI dev. | unset |

Build and run:

```sh
SQLX_OFFLINE=true cargo build --release -p squadron
DATABASE_URL=sqlite:$PWD/dev.db?mode=rwc DATA_DIR=$PWD/work OPENCODE_KEY=sk-... \
  ./target/release/squadron
```

Then open `http://localhost:3000` and submit a brief. Run status, lanes,
validator reports, screenshots, and the verdict all stream in on the run page.

### Local dev without real sprites

The `sprites-client` crate ships a mock `sprite` CLI (`mock-sprite-cli`) and a
compiled fixture binary, so a full pipeline runs on your laptop with no
network, no Fly account, and no Chromium:

```sh
cargo build -p sprites-client --bins
SPRITE_CLI=$PWD/target/debug/mock-sprite-cli \
SQUADRON_TEST_MODE=1 SQUADRON_FIXTURES=1 \
DATABASE_URL=sqlite:$PWD/dev.db?mode=rwc DATA_DIR=$PWD/work \
  cargo run -p squadron
```

`SQUADRON_TEST_MODE=1` short-circuits the HTTP 200-checks and judge screenshot
extraction; canned implementer/validator JSON passes the schema gate, and the
judge uses its schema-valid fallback verdict. `SQUADRON_FIXTURES=1` additionally
exposes pre-rendered example runs for dashboard styling.

## Deploy

Deploy targets Fly.io: the Dockerfile builds the release binary multi-stage,
then ships a Debian runtime with Chromium + fonts (for lane screenshots), the
`sprite` CLI (official installer), and `tokei` prebuilt. The Fly app mounts a
persistent volume at `/data` for SQLite and artifacts (see `fly.toml`).

```sh
fly launch        # or: fly apps create squadron
fly volumes create squadron_data --size 10
fly secrets set OPENCODE_KEY=... SPRITE_TOKEN=...
fly deploy
```

Lane sprites are provisioned at runtime through the `sprite` CLI (the
`SPRITE_TOKEN` secret authenticates the boot step); nothing else needs to be
manually created.

## What I got wrong and fixed

Honest list of the failure classes hit while building this, and the fixes:

- Run IDs are case-sensitive capability tokens, but the sprite CLI only accepts
  `^[a-z0-9][a-z0-9-]*$` names — sprite names now lowercase/strip the run-ID
  segment before building `squadron-<run>-impl-<i>`.
- Sprite auto-sleep killed what looked like "detached" long-running sessions —
  a supervisor pattern (keep-alive pinned to the sprite for the duration of
  lane work) fixed it.
- Implementer dev servers died the moment the harness session ended, so the
  public 200-check always failed — agents now follow a detached-serving
  protocol (nohup-style serving that outlives the agent process).
- Agents that bound to loopback produced servers unreachable via the assigned
  public `*.sprites.app` URL — prompts now require binding to `0.0.0.0` on the
  assigned public URL, verified by the 200-check.
- Interrupted harness runs left 0-byte output files with the useful content
  lost — the runner now captures a tail of the last partial response instead
  of failing on empty output.
- The gateway sometimes answered with reasoning-only responses (no wire
  content) on the curl floor — fixed with retry-at-lower-reasoning-effort and
  raw-response logging to diagnose the shape.
- The smoke test timed out at 30 s even on clean main because test mode still
  performed real HTTP probes against fake sprite URLs — test mode now
  short-circuits all network paths, and liveness gates no longer stall.
- Codex 0.167 dropped the chat-completions wire API (Responses-API only), so it
  cannot speak the gateway recipe — blacklisted as a harness choice.
- `tokio::process::Command::wait_with_output()` captured nothing because
  stdio defaulted to inherited — explicit `Stdio::piped()` on both streams
  (symptom: `sprite list` returned `[]` with empty stderr).
- SQLx type inference surprises: `TEXT PRIMARY KEY` inferred as
  `Option<String>`, `COALESCE(MAX(ts),0)` as non-null — mapped explicitly
  instead of trusting inference.
- Concurrent SQLite writers deadlocked (`SQLITE_BUSY`) with sqlx's deferred
  `BEGIN` — both event/cost paths now use `BEGIN IMMEDIATE`.
- The harness-choice cache was process-wide, so lane 0's probe result was
  reused for every other sprite — the cache is keyed per sprite name.
- Harness-attributable failures burned the scarce lane-retry budget — retries
  now use a separate harness budget that walks the chain down to the curl
  floor, with failed choices blacklisted.
- Costs were double-counted (agent-reported usage inside its own JSON vs the
  gateway-reported run cost) — only gateway-reported costs are summed; unknown
  components persist as NULL and render as `-`.
- A validator `verdict: fail` still left the lane `validated`, so failing
  lanes could win — failing lanes are demoted to failed-with-report and
  excluded from the judge set.
- Manual state events and a `judged` lane transition fought the spec's lane
  state machine — all transitions funnel through one `transition()` append.
- Shell-script test fixtures flaked with `ETXTBSY` under parallel tests on
  overlayfs — replaced with a compiled fixture binary symlinked per tempdir.
- Prompt injection was only schema-checked — a `[INJECTION?]` marker scan now
  runs with the schema gate, and any marker fails the lane hard.
- The mock CLI's HTTP 200 probe hung forever on a listening-but-silent port —
  bounded `connect_timeout` plus read timeout.

## Harness choice

Each lane needs a coding-agent harness to run the implementer/validator
prompts on its sprite. The chain (first available wins, per-sprite probed and
cached, escalating on harness-attributable failure):

1. **opencode** — the default. Installs in-lane (`curl -fsSL https://opencode.ai/install | bash`), configured through an `opencode.json` that points it at the no-auth LLM gateway recipe (stable session, model constants, `reasoning_effort=low`), and its final JSON output is schema-gated like every other path.
2. **claude** — second fallback if opencode is unavailable on that sprite.
3. **curl-loop floor** — guaranteed last resort: a plain `curl` loop against the gateway's chat endpoint with a 12-turn budget. It has no shell access, so the orchestrator materializes the agent's declared files itself; this keeps a lane buildable even with zero agent CLIs installed.

Why not codex: codex 0.167 removed the chat-completions wire API and speaks
only the Responses API, which the gateway recipe cannot emulate — it is
blacklisted (`HarnessChoice::Codex` is never probed).

Why opencode won: it is a single self-contained binary, installs without root,
runs headless with a config-file recipe (no interactive auth), emits
machine-parseable output, and its model/endpoint are fully controlled by the
orchestrator. The curl floor exists because "no agent CLI available" must
still produce a schema-valid implementer output, not a dead lane.

## License

MIT.
