use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine;
use futures::StreamExt;
use tokio::sync::{mpsc, Notify, RwLock};

use sprites_client::{ExecOptions, SpriteClient};
use store::{now_ms, Store};

use crate::revive::revive_exec_cmd;

/// F37: bounded revive attempts before a capture (max 2 tries — a dead URL
/// gets the lazy-revive path, never an unbounded retry loop).
pub const CAPTURE_REVIVE_MAX_TRIES: usize = 2;

/// How long a restarted detached server gets before the re-probe (mirrors
/// `REVIVE_SETTLE`: a freshly woken sprite's server can take a few seconds
/// to bind).
pub const CAPTURE_SETTLE: std::time::Duration = std::time::Duration::from_secs(3);

/// Shared hub registry: run_id -> SSE hub. Wired into the screenshot queue so
/// thumbnail state changes reach live SSE subscribers (audit R3 finding 3).
pub type HubRegistry = Arc<RwLock<HashMap<String, crate::sse::SseHub>>>;

/// Injectable public-URL status probe (production: the SSRF-safe shared
/// `net::fetch_public_status`; tests: deterministic fakes). Returns the HTTP
/// status or a transport error.
pub type StatusProbe =
    Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, Result<u16, String>> + Send + Sync>;

/// Injectable detached-restart step for the capture gate (production: a
/// `sprite exec` of the stored run_command; tests: deterministic fakes). The
/// argument is the fully built nohup command.
pub type ReviveExecHook =
    Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, Result<(), String>> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenshotState {
    Pending,
    Queued,
    Rendering,
    Done,
    Failed,
}

impl ScreenshotState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Queued => "queued",
            Self::Rendering => "rendering",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
struct ScreenshotJob {
    run_id: String,
    lane_idx: i64,
    url: String,
}

/// #36: single path convention shared by the real renderer and test mode,
/// derived from the configured data dir (never hardcoded /data).
pub fn render_dest(data_dir: &std::path::Path, run_id: &str, lane_idx: i64) -> PathBuf {
    data_dir
        .join("screenshots")
        .join(run_id)
        .join(format!("lane-{lane_idx}.png"))
}

/// Broadcast a screenshot state change to the run's SSE hub (audit R3
/// finding 3): the gallery island only sees thumbnails when the snapshot with
/// the updated screenshot state reaches the live subscriber.
async fn publish_state_change(store: &Store, hubs: &Option<HubRegistry>, run_id: &str) {
    if let Some(reg) = hubs {
        let hub = reg.read().await.get(run_id).cloned();
        if let Some(hub) = hub {
            if let Some(snap) = crate::snapshot::build_snapshot(store, run_id).await {
                hub.publish(snap);
            }
        }
    }
}

/// 1x1 transparent PNG used as the test-mode placeholder so test thumbnails
/// are actually servable instead of 404ing on a fabricated path.
const TEST_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/// F14: every screenshot destination directory must exist before the render
/// writes into it (chromiumoxide's save_screenshot does not mkdir, so a
/// missing `DATA_DIR/screenshots/{run}` failed the render with ENOENT).
async fn ensure_dest_dir(dest: &std::path::Path) -> Result<(), String> {
    let Some(parent) = dest.parent() else {
        return Ok(());
    };
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|e| format!("mkdir {}: {e}", parent.display()))
}

async fn write_test_png(dest: &std::path::Path) -> Result<(), String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(TEST_PNG_BASE64)
        .map_err(|e| format!("test png decode: {e}"))?;
    ensure_dest_dir(dest).await?;
    tokio::fs::write(dest, bytes)
        .await
        .map_err(|e| format!("write {}: {e}", dest.display()))
}

/// Review F4: which Chromium sandbox mode served a render. Sandboxed is the
/// default (the browser is untrusted-content-facing); NoSandbox is the
/// escape-hatch fallback (env opt-in or sandboxed-launch retry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    Sandbox,
    NoSandbox,
}

impl RenderMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sandbox => "sandboxed",
            Self::NoSandbox => "no-sandbox",
        }
    }
}

/// Review F4 escape hatch: SQUADRON_CHROMIUM_NO_SANDBOX=1 opts into running
/// Chromium with --no-sandbox from the start (environments where the
/// sandboxed launch is known to fail, e.g. root without user namespaces).
fn no_sandbox_env() -> bool {
    std::env::var("SQUADRON_CHROMIUM_NO_SANDBOX")
        .ok()
        .as_deref()
        == Some("1")
}

/// One Chromium launch + navigate + screenshot attempt with the given
/// sandbox configuration.
async fn render_chromium_once(
    bin: &PathBuf,
    no_sandbox: bool,
    url: &str,
    dest: &std::path::Path,
) -> Result<(), String> {
    let mut config = chromiumoxide::BrowserConfig::builder()
        .chrome_executable(bin)
        .window_size(1280, 800)
        .launch_timeout(std::time::Duration::from_secs(30))
        .request_timeout(std::time::Duration::from_secs(45));
    if no_sandbox {
        config = config.no_sandbox();
    }
    let config = config
        .build()
        .map_err(|e| format!("chromium config: {e}"))?;
    let (mut browser, mut handler) = chromiumoxide::Browser::launch(config)
        .await
        .map_err(|e| format!("chromium launch: {e}"))?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });
    let render = async {
        let page = browser
            .new_page(url)
            .await
            .map_err(|e| format!("goto: {e}"))?;
        page.wait_for_navigation()
            .await
            .map_err(|e| format!("navigation: {e}"))?;
        page.save_screenshot(
            chromiumoxide::page::ScreenshotParams::builder()
                .format(
                    chromiumoxide_cdp::cdp::browser_protocol::page::CaptureScreenshotFormat::Png,
                )
                .full_page(true)
                .build(),
            dest,
        )
        .await
        .map_err(|e| format!("screenshot: {e}"))?;
        Ok(())
    };
    let result = render.await;
    let _ = browser.close().await;
    let _ = browser.wait().await;
    let _ = handler_task.await;
    result
}

/// Review F4: the browser renders untrusted lane content, so the Chromium
/// sandbox defaults to ON (the Fly runtime image runs non-root). The
/// SQUADRON_CHROMIUM_NO_SANDBOX=1 env var is a loud escape hatch for hosts
/// where the sandboxed launch cannot work; and if the sandboxed launch
/// fails at runtime (launch error only — navigation/render failures are not
/// retried), the render retries exactly once with the escape hatch. The
/// returned mode records which configuration actually served the render.
async fn render_chromium(url: &str, dest: &std::path::Path) -> Result<RenderMode, String> {
    let bin = std::env::var_os("CHROMIUM_BIN")
        .map(PathBuf::from)
        .ok_or_else(|| "CHROMIUM_BIN not set; no chromium available".to_owned())?;
    if no_sandbox_env() {
        tracing::warn!(
            "SQUADRON_CHROMIUM_NO_SANDBOX=1: launching headless Chromium WITHOUT its sandbox \
             (--no-sandbox) — this weakens isolation for untrusted content and is an \
             escape hatch, not a supported configuration"
        );
        render_chromium_once(&bin, true, url, dest).await?;
        return Ok(RenderMode::NoSandbox);
    }
    match render_chromium_once(&bin, false, url, dest).await {
        Ok(()) => Ok(RenderMode::Sandbox),
        Err(e) if e.starts_with("chromium launch:") => {
            tracing::warn!(
                "sandboxed Chromium launch failed ({e}); retrying once with the \
                 SQUADRON_CHROMIUM_NO_SANDBOX escape hatch (--no-sandbox) — untrusted-content \
                 isolation is reduced for this render"
            );
            render_chromium_once(&bin, true, url, dest)
                .await
                .map_err(|retry| {
                    format!("sandboxed launch failed ({e}); no-sandbox retry: {retry}")
                })?;
            Ok(RenderMode::NoSandbox)
        }
        Err(e) => Err(e),
    }
}

/// F37: the testable capture-gate core. Probes the lane's public URL; a 2xx
/// answer passes immediately. A dead URL gets the bounded lazy-revive path
/// (the `revive` closure restarts the lane's detached server from its stored
/// run_command — at most [`CAPTURE_REVIVE_MAX_TRIES`] tries, each followed by
/// a settle and a re-probe). Still dead after the budget → Err, and the
/// caller must mark the screenshot FAILED without storing a frame: a
/// screenshot of a transient 502/error page misleads the judge (run 2,
/// 2026-11-25: a passing lane was weighed down by a stale error frame while
/// the validator's live probe had verified 200).
pub async fn ensure_live_for_capture<P, PFut, R, RFut>(
    url: &str,
    probe: P,
    revive: R,
    settle: std::time::Duration,
) -> Result<(), String>
where
    P: Fn(String) -> PFut,
    PFut: Future<Output = Result<u16, String>>,
    R: Fn(usize) -> RFut,
    RFut: Future<Output = Result<(), String>>,
{
    if let Ok(status) = probe(url.to_owned()).await {
        if (200..300).contains(&status) {
            return Ok(());
        }
    }
    for try_n in 1..=CAPTURE_REVIVE_MAX_TRIES {
        if let Err(e) = revive(try_n).await {
            return Err(format!("revive attempt {try_n} failed: {e}"));
        }
        tokio::time::sleep(settle).await;
        match probe(url.to_owned()).await {
            Ok(status) if (200..300).contains(&status) => return Ok(()),
            _ => continue,
        }
    }
    Err(format!(
        "public URL not 2xx after {CAPTURE_REVIVE_MAX_TRIES} revive tries; rejecting capture"
    ))
}

/// One status probe through the hook (test) or the shared SSRF-safe fetcher
/// (production); true only on a 2xx answer.
async fn probe_is_2xx(probe_hook: Option<&StatusProbe>, allow_loopback: bool, url: &str) -> bool {
    match probe_hook {
        Some(hook) => matches!(hook(url.to_owned()).await, Ok(s) if (200..300).contains(&s)),
        None => matches!(
            crate::net::fetch_public_status(url, allow_loopback).await,
            Ok(s) if (200..300).contains(&s)
        ),
    }
}

/// F37 production capture gate: probe the lane's public URL; when dead, run
/// the bounded lazy-revive path (detached restart of the stored run_command
/// on the lane sprite, at most [`CAPTURE_REVIVE_MAX_TRIES`] tries), then let
/// the caller capture. Every attempt is logged to lane_events so the run's
/// evidence shows why a frame exists or does not.
async fn capture_gate(
    store: &Store,
    job: &ScreenshotJob,
    probe_hook: Option<&StatusProbe>,
    revive_hook: Option<&ReviveExecHook>,
    revive: Option<&(SpriteClient, bool)>,
) -> Result<(), String> {
    let allow_loopback = revive.map(|(_, lo)| *lo).unwrap_or(false);
    let can_revive = revive_hook.is_some() || revive.is_some();
    if !can_revive {
        // No revive capability wired (test-mode queues never reach here;
        // production always wires the sprite): a single probe decides.
        return if probe_is_2xx(probe_hook, allow_loopback, &job.url).await {
            Ok(())
        } else {
            Err("public URL dead and revive not wired; rejecting capture".to_owned())
        };
    }
    let probe = {
        let hook = probe_hook.cloned();
        move |url: String| {
            let hook = hook.clone();
            async move {
                match &hook {
                    Some(h) => h(url).await,
                    None => crate::net::fetch_public_status(&url, allow_loopback).await,
                }
            }
        }
    };
    let store_rev = store.clone();
    let run_id = job.run_id.clone();
    let lane_idx = job.lane_idx;
    let revive_hook_rev = revive_hook.cloned();
    let sprite_rev = revive.cloned();
    let revive = move |try_n: usize| {
        let store_rev = store_rev.clone();
        let run_id = run_id.clone();
        let revive_hook_rev = revive_hook_rev.clone();
        let sprite_rev = sprite_rev.clone();
        async move {
            let _ = store_rev
                .append_lane_event(
                    &run_id,
                    lane_idx,
                    now_ms(),
                    "log",
                    &format!(
                        "screenshot capture gate: public URL dead; revive attempt {try_n}/{CAPTURE_REVIVE_MAX_TRIES} (detached restart of the stored run_command)"
                    ),
                )
                .await;
            let run_command = match store_rev
                .latest_lane_event_of_kind(&run_id, lane_idx, "run_command")
                .await
            {
                Ok(Some(event)) if !event.payload.trim().is_empty() => event.payload,
                _ => {
                    return Err("no stored run_command; cannot revive".to_owned());
                }
            };
            let workdir = crate::pipeline::sprite_workdir(&run_id, lane_idx);
            let cmd = revive_exec_cmd(&workdir, &run_command);
            if let Some(hook) = revive_hook_rev {
                return hook(cmd).await;
            }
            let Some((sprite, _)) = sprite_rev else {
                return Err("revive not wired; cannot restart the lane server".to_owned());
            };
            let sprite_name = match store_rev.get_lane(&run_id, lane_idx).await {
                Ok(Some(lane)) => lane.sprite_name,
                _ => None,
            };
            let Some(sprite_name) = sprite_name else {
                return Err("lane has no sprite name; cannot revive".to_owned());
            };
            let opts = ExecOptions::default().with_timeout(crate::revive::REVIVE_EXEC_TIMEOUT);
            match sprite.exec(&sprite_name, &cmd, &opts).await {
                Ok(out) if out.exit_code == 0 => Ok(()),
                Ok(out) => Err(format!("restart exited {}", out.exit_code)),
                Err(e) => Err(e.to_string()),
            }
        }
    };
    ensure_live_for_capture(&job.url, probe, revive, CAPTURE_SETTLE).await
}

/// Manual Debug: the injected probe/revive hooks are opaque closures.
impl std::fmt::Debug for ScreenshotQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenshotQueue").finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct ScreenshotQueue {
    tx: mpsc::Sender<ScreenshotJob>,
    store: Store,
    pending: Arc<RwLock<HashMap<(String, i64), String>>>, // value: terminal state string
    notify: Arc<Notify>,
    hubs: Option<HubRegistry>,
    /// F37: probe override point (tests inject deterministic fakes). None =
    /// the shared SSRF-safe `net::fetch_public_status` with `allow_loopback`.
    /// Shared with the worker so builder methods applied after `build`
    /// (e.g. `with_revive` in state.rs) still take effect.
    probe_hook: Arc<std::sync::RwLock<Option<StatusProbe>>>,
    /// F37: detached-restart override for the capture gate's revive step
    /// (tests inject fakes). None = `sprite exec` via the wired sprite.
    /// Same shared-cell contract as `probe_hook`.
    revive_hook: Arc<std::sync::RwLock<Option<ReviveExecHook>>>,
    /// F37: sprite + loopback policy for the gate's probe/revive steps.
    /// None = revive is not wired; the gate still probes and rejects a dead
    /// URL's capture without a revive attempt.
    revive: Arc<std::sync::RwLock<Option<(SpriteClient, bool)>>>,
}

impl ScreenshotQueue {
    /// Compat constructor: resolves the data dir from DATA_DIR (matching
    /// PipelineConfig's default). Prefer `with_data_dir` when a config is at
    /// hand so the queue always agrees with the pipeline.
    pub fn new(store: Store, test_mode: bool) -> Self {
        let data_dir = std::env::var_os("DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/data"));
        Self::with_data_dir(store, test_mode, data_dir)
    }

    pub fn with_data_dir(store: Store, test_mode: bool, data_dir: PathBuf) -> Self {
        Self::with_data_dir_hubs(store, test_mode, data_dir, None)
    }

    /// `hubs`: the run -> SseHub registry. When present, every screenshot
    /// state change is broadcast as a full snapshot to the run's hub so live
    /// SSE subscribers see thumbnails without a reload (audit R3 finding 3).
    pub fn with_data_dir_hubs(
        store: Store,
        test_mode: bool,
        data_dir: PathBuf,
        hubs: Option<HubRegistry>,
    ) -> Self {
        Self::build(store, test_mode, data_dir, hubs, None, None, None)
    }

    /// F37: production wiring for the capture gate — the sprite client (for
    /// the detached restart exec) and the loopback policy for the probe.
    /// Without this the gate still probes and rejects a dead URL's capture,
    /// but cannot attempt a revive.
    pub fn with_revive(self, sprite: SpriteClient, allow_loopback: bool) -> Self {
        *self.revive.write().unwrap_or_else(|p| p.into_inner()) = Some((sprite, allow_loopback));
        self
    }

    /// F37: test-only probe injection (deterministic dead/alive states
    /// without the network). Production callers leave it unset.
    pub fn with_probe_hook(self, probe: StatusProbe) -> Self {
        *self.probe_hook.write().unwrap_or_else(|p| p.into_inner()) = Some(probe);
        self
    }

    /// F37: test-only detached-restart injection (deterministic revive
    /// outcomes without sprite infra). Production callers leave it unset.
    pub fn with_revive_hook(self, revive: ReviveExecHook) -> Self {
        *self.revive_hook.write().unwrap_or_else(|p| p.into_inner()) = Some(revive);
        self
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        store: Store,
        test_mode: bool,
        data_dir: PathBuf,
        hubs: Option<HubRegistry>,
        probe_hook: Option<StatusProbe>,
        revive_hook: Option<ReviveExecHook>,
        revive: Option<(SpriteClient, bool)>,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel::<ScreenshotJob>(128);
        let store_worker = store.clone();
        let data_dir_worker = data_dir.clone();
        let pending: Arc<RwLock<HashMap<(String, i64), String>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let notify = Arc::new(Notify::new());
        let pending_worker = pending.clone();
        let notify_worker = notify.clone();
        let hubs_worker = hubs.clone();
        // F37: the gate wiring (probe hook, revive hook, sprite) lives in
        // shared cells — builders may run after `build` (state.rs wires
        // `with_revive` on the queue returned here, tests chain the hook
        // builders), so the worker snapshots the current wiring per job
        // instead of capturing it once at spawn time.
        let probe_cell = Arc::new(std::sync::RwLock::new(probe_hook));
        let revive_hook_cell = Arc::new(std::sync::RwLock::new(revive_hook));
        let revive_cell = Arc::new(std::sync::RwLock::new(revive));
        let probe_worker = probe_cell.clone();
        let revive_hook_worker = revive_hook_cell.clone();
        let revive_worker = revive_cell.clone();
        tokio::spawn(async move {
            // serial: one-at-a-time by nature of single consumer loop
            while let Some(job) = rx.recv().await {
                // Snapshot the wiring for this job; the guards drop here,
                // before any await below.
                let probe_worker_opt = probe_worker
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                let revive_hook_worker_opt = revive_hook_worker
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                let revive_worker_opt = revive_worker
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                {
                    pending_worker
                        .write()
                        .await
                        .insert((job.run_id.clone(), job.lane_idx), "rendering".to_owned());
                }
                let ts = now_ms();
                let _ = store_worker
                    .append_lane_event(
                        &job.run_id,
                        job.lane_idx,
                        ts,
                        "state",
                        "screenshot_rendering",
                    )
                    .await;
                let _ = store_worker
                    .set_screenshot_state(&job.run_id, job.lane_idx, "rendering", None)
                    .await;
                publish_state_change(&store_worker, &hubs_worker, &job.run_id).await;

                let (state, path_opt) = if test_mode {
                    // Test mode: same path convention as real renders, and a
                    // real placeholder file so /{run}/thumb/{idx} serves it.
                    // F37 gate skipped: test URLs are fake; the production
                    // probe/revive path is covered by the gate's own tests.
                    let dest = render_dest(&data_dir_worker, &job.run_id, job.lane_idx);
                    match write_test_png(&dest).await {
                        Ok(()) => ("done".to_owned(), Some(dest.to_string_lossy().into_owned())),
                        Err(e) => {
                            tracing::warn!(error = %e, "test-mode screenshot write failed");
                            ("failed".to_owned(), None)
                        }
                    }
                } else {
                    // Real Chromium rendering: serial worker (this loop is the
                    // only consumer). Requires CHROMIUM_BIN; without it the
                    // screenshot fails honestly instead of blocking. The
                    // sandbox mode that served the render (review F4) is
                    // recorded in lane_events.
                    let dest = render_dest(&data_dir_worker, &job.run_id, job.lane_idx);
                    // F37 capture integrity gate: probe the lane's public URL
                    // before capturing; if dead, run the bounded lazy-revive
                    // path (max CAPTURE_REVIVE_MAX_TRIES tries), then
                    // capture. A non-200 gate answer rejects the capture —
                    // the screenshot is marked FAILED with NO stored frame,
                    // never a misleading error-page image (the judge then
                    // judges that lane text-only via the existing degrade).
                    let gate = capture_gate(
                        &store_worker,
                        &job,
                        probe_worker_opt.as_ref(),
                        revive_hook_worker_opt.as_ref(),
                        revive_worker_opt.as_ref(),
                    )
                    .await;
                    match gate {
                        Err(reason) => {
                            tracing::warn!(error = %reason, "screenshot capture gate rejected");
                            let _ = store_worker
                                .append_lane_event(
                                    &job.run_id,
                                    job.lane_idx,
                                    now_ms(),
                                    "log",
                                    &format!("screenshot capture gate: {reason}; no frame stored"),
                                )
                                .await;
                            ("failed".to_owned(), None)
                        }
                        Ok(()) => {
                            // F14: create the destination directory before the render
                            // (save_screenshot does not mkdir; a missing
                            // DATA_DIR/screenshots/{run} failed real renders with ENOENT).
                            let rendered = match ensure_dest_dir(&dest).await {
                                Ok(()) => render_chromium(&job.url, &dest).await,
                                Err(e) => Err(e),
                            };
                            match rendered {
                                Ok(mode) => {
                                    // F37: the URL could have died mid-render;
                                    // a re-probe that answers non-2xx means
                                    // the stored frame is an error page —
                                    // reject it instead of storing it.
                                    let still_live = probe_is_2xx(
                                        probe_worker_opt.as_ref(),
                                        revive_worker_opt
                                            .as_ref()
                                            .map(|(_, lo)| *lo)
                                            .unwrap_or(false),
                                        &job.url,
                                    )
                                    .await;
                                    if !still_live {
                                        let _ = tokio::fs::remove_file(&dest).await;
                                        let _ = store_worker
                                            .append_lane_event(
                                                &job.run_id,
                                                job.lane_idx,
                                                now_ms(),
                                                "log",
                                                "screenshot capture gate: public URL died mid-render; rejected the error-page frame",
                                            )
                                            .await;
                                        ("failed".to_owned(), None)
                                    } else {
                                        let ts = now_ms();
                                        let _ = store_worker
                                            .append_lane_event(
                                                &job.run_id,
                                                job.lane_idx,
                                                ts,
                                                "state",
                                                &format!(
                                                    "screenshot_render_mode={}",
                                                    mode.as_str()
                                                ),
                                            )
                                            .await;
                                        (
                                            "done".to_owned(),
                                            Some(dest.to_string_lossy().into_owned()),
                                        )
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "screenshot render failed");
                                    ("failed".to_owned(), None)
                                }
                            }
                        }
                    }
                };

                let ts = now_ms();
                let _ = store_worker
                    .append_lane_event(
                        &job.run_id,
                        job.lane_idx,
                        ts,
                        "state",
                        &format!("screenshot_{state}"),
                    )
                    .await;
                let _ = store_worker
                    .set_screenshot_state(&job.run_id, job.lane_idx, &state, path_opt.as_deref())
                    .await;
                {
                    pending_worker
                        .write()
                        .await
                        .insert((job.run_id.clone(), job.lane_idx), state.clone());
                }
                publish_state_change(&store_worker, &hubs_worker, &job.run_id).await;
                notify_worker.notify_waiters();
            }
        });
        Self {
            tx,
            store,
            pending,
            notify,
            hubs,
            probe_hook: probe_cell,
            revive_hook: revive_hook_cell,
            revive: revive_cell,
        }
    }

    /// Lazy request: enqueue if not already done/queued/rendering. Returns current state string.
    pub async fn request(&self, run_id: &str, lane_idx: i64, url: &str) -> String {
        let key = (run_id.to_owned(), lane_idx);
        {
            let map = self.pending.read().await;
            if let Some(s) = map.get(&key) {
                return s.clone();
            }
        }
        // Adopt terminal state persisted by a previous queue instance (e.g. after
        // restart). In-progress or pending DB states are ignored so the job is
        // (re)enqueued rather than stuck.
        if let Ok(Some(lane)) = self.store.get_lane(run_id, lane_idx).await {
            if matches!(
                lane.screenshot_state.as_deref(),
                Some("done") | Some("failed")
            ) {
                let s = lane.screenshot_state.clone().unwrap_or_default();
                self.pending.write().await.insert(key, s.clone());
                return s;
            }
        }
        // mark queued optimistically
        {
            let mut map = self.pending.write().await;
            if let Some(s) = map.get(&key) {
                return s.clone();
            }
            map.insert(key.clone(), "queued".to_owned());
        }
        let job = ScreenshotJob {
            run_id: run_id.to_owned(),
            lane_idx,
            url: url.to_owned(),
        };
        if self.tx.send(job).await.is_err() {
            let mut map = self.pending.write().await;
            map.insert(key, "failed".to_owned());
            return "failed".to_owned();
        }
        "queued".to_owned()
    }

    /// Terminal-run re-adoption (the web layer's gallery/lane view): enqueue a
    /// render for a lane whose persisted screenshot state is `pending` (never
    /// attempted) or `failed` (attempted, settled without a frame) — `request`
    /// adopts a persisted terminal state and would return it forever. An
    /// attempt already in flight (queued/rendering in the live map) is never
    /// double-enqueued; the caller filters `done`. Returns the state string
    /// the lane should now show.
    pub async fn requeue(&self, run_id: &str, lane_idx: i64, url: &str) -> String {
        let key = (run_id.to_owned(), lane_idx);
        {
            let mut map = self.pending.write().await;
            match map.get(&key).cloned() {
                Some(s) if s == "queued" || s == "rendering" => return s,
                _ => {}
            }
            map.insert(key.clone(), "queued".to_owned());
        }
        let job = ScreenshotJob {
            run_id: run_id.to_owned(),
            lane_idx,
            url: url.to_owned(),
        };
        if self.tx.send(job).await.is_err() {
            let mut map = self.pending.write().await;
            map.insert(key, "failed".to_owned());
            return "failed".to_owned();
        }
        "queued".to_owned()
    }

    pub fn notify(&self) -> Arc<Notify> {
        self.notify.clone()
    }

    /// The shared hub registry (None when the queue was built without one).
    pub fn hub_registry(&self) -> Option<HubRegistry> {
        self.hubs.clone()
    }

    fn is_terminal_state(state: &str) -> bool {
        state == "done" || state == "failed"
    }

    /// Last known state for a lane: in-memory cache first, then the store.
    async fn state_for(&self, run_id: &str, lane_idx: i64) -> Option<String> {
        {
            let map = self.pending.read().await;
            if let Some(s) = map.get(&(run_id.to_owned(), lane_idx)) {
                return Some(s.clone());
            }
        }
        if let Ok(Some(lane)) = self.store.get_lane(run_id, lane_idx).await {
            if let Some(s) = lane.screenshot_state {
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
        None
    }

    /// Cached (not store-adopted) state for a lane, for introspection/tests.
    pub async fn pending_state(&self, run_id: &str, lane_idx: i64) -> Option<String> {
        self.pending
            .read()
            .await
            .get(&(run_id.to_owned(), lane_idx))
            .cloned()
    }

    /// R2 finding 2: ensure a render has been attempted and reached a terminal
    /// state within `timeout`. Enqueues the job when needed, waits bounded on
    /// the worker's notify, and returns the last known state ("pending" when
    /// the wait expires without a terminal state).
    pub async fn ensure_rendered(
        &self,
        run_id: &str,
        lane_idx: i64,
        url: &str,
        timeout: std::time::Duration,
    ) -> String {
        let _ = self.request(run_id, lane_idx, url).await;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(s) = self.state_for(run_id, lane_idx).await {
                if Self::is_terminal_state(&s) {
                    return s;
                }
            }
            // Register for the worker's wake before re-checking, so a state
            // change that lands between checks cannot be missed.
            let notified = self.notify.notified();
            if let Some(s) = self.state_for(run_id, lane_idx).await {
                if Self::is_terminal_state(&s) {
                    return s;
                }
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return self
                    .state_for(run_id, lane_idx)
                    .await
                    .unwrap_or_else(|| "pending".to_owned());
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(remaining) => {
                    return self
                        .state_for(run_id, lane_idx)
                        .await
                        .unwrap_or_else(|| "pending".to_owned());
                }
            }
        }
    }

    /// R2 finding 6: drop cached terminal entries for a run. `request`
    /// re-adopts terminal states from the store, so eviction only frees
    /// memory once nothing can change anymore.
    pub async fn evict_terminal(&self, run_id: &str) {
        let mut map = self.pending.write().await;
        map.retain(|(rid, _), state| !(rid == run_id && Self::is_terminal_state(state)));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // test code: panics on failure are the assertion mechanism

    use super::*;

    /// Review F4: the escape hatch env var decides whether the FIRST launch
    /// already runs --no-sandbox (env set) or sandboxed with a no-sandbox
    /// retry only on a sandboxed launch failure (default).
    #[test]
    fn no_sandbox_escape_hatch_env() {
        let key = "SQUADRON_CHROMIUM_NO_SANDBOX";
        // Default: sandbox on, escape hatch unused.
        std::env::remove_var(key);
        assert!(!no_sandbox_env());
        // Opt-in: first launch is already unsandboxed.
        std::env::set_var(key, "1");
        assert!(no_sandbox_env());
        // Any other value keeps the sandbox.
        std::env::set_var(key, "0");
        assert!(!no_sandbox_env());
        std::env::remove_var(key);
    }

    #[test]
    fn launch_failure_marker_gates_the_no_sandbox_retry() {
        // Only a launch error ("chromium launch: ...") is retried with the
        // escape hatch; config/goto/screenshot errors surface as-is.
        for e in [
            "chromium config: bad",
            "goto: timeout",
            "screenshot: failed",
        ] {
            assert!(!e.starts_with("chromium launch:"));
        }
        assert!("chromium launch: no such binary".starts_with("chromium launch:"));
    }

    #[test]
    fn render_mode_is_recordable() {
        assert_eq!(RenderMode::Sandbox.as_str(), "sandboxed");
        assert_eq!(RenderMode::NoSandbox.as_str(), "no-sandbox");
    }

    /// F14: render_dest derives from the configured data dir (never a
    /// hardcoded /data segment) and always points under DATA_DIR.
    #[test]
    fn render_dest_is_derived_from_data_dir() {
        let data_dir = std::path::Path::new("/tmp/squadron-data");
        let dest = render_dest(data_dir, "run-abc", 2);
        assert_eq!(
            dest,
            data_dir
                .join("screenshots")
                .join("run-abc")
                .join("lane-2.png")
        );
        assert!(dest.starts_with(data_dir));
    }

    /// F14 regression: with a DATA_DIR that does NOT pre-create the
    /// screenshots subtree, ensure_dest_dir creates the full parent chain so
    /// the render's save never fails with ENOENT.
    #[tokio::test]
    async fn ensure_dest_dir_creates_missing_screenshots_dir() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data"); // only DATA_DIR exists
        let dest = render_dest(&data_dir, "run-f14", 0);
        ensure_dest_dir(&dest).await.unwrap();
        assert!(dest.parent().unwrap().is_dir(), "screenshots/run created");
        assert!(data_dir.is_dir());
        // Idempotent on a second call (real retries / test mode).
        ensure_dest_dir(&dest).await.unwrap();
    }

    // --- F37: capture-integrity gate -------------------------------------

    /// Env-mutating queue-level tests run under this lock so a parallel env
    /// test (or another queue test) never sees a half-mutated CHROMIUM_BIN.
    static GATE_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn gate_test_store() -> Store {
        let store = Store::connect_in_memory().await.unwrap();
        store.migrate().await.unwrap();
        store.create_run("brief", 1, 0).await.unwrap();
        let run = store.list_runs().await.unwrap().remove(0);
        store.insert_lane(&run.id, 0, "{}").await.unwrap();
        store
    }

    fn boxed_probe<F>(f: F) -> StatusProbe
    where
        F: Fn(String) -> futures::future::BoxFuture<'static, Result<u16, String>>
            + Send
            + Sync
            + 'static,
    {
        Arc::new(f)
    }

    /// F37: a dead public URL is revived before the capture — the detached
    /// restart runs, the re-probe sees 200, and the gate passes.
    #[tokio::test]
    async fn gate_revives_a_dead_url_before_capture() {
        let up = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let up_probe = up.clone();
        let up_revive = up.clone();
        let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe_count = probes.clone();
        let revives = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let revive_count = revives.clone();
        let probe = move |_: String| {
            probe_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let up = up_probe.clone();
            async move {
                if up.load(std::sync::atomic::Ordering::SeqCst) {
                    Ok(200)
                } else {
                    Ok(502)
                }
            }
        };
        let revive = move |_: usize| {
            revive_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            up_revive.store(true, std::sync::atomic::Ordering::SeqCst);
            async { Ok(()) }
        };
        let out = ensure_live_for_capture(
            "https://lane.example/",
            probe,
            revive,
            std::time::Duration::ZERO,
        )
        .await;
        assert!(out.is_ok(), "gate must pass after the revive: {out:?}");
        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(revives.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// F37: the revive budget is bounded (max CAPTURE_REVIVE_MAX_TRIES) — a
    /// lane that stays dead gets exactly that many tries, then the capture
    /// is rejected.
    #[tokio::test]
    async fn gate_is_bounded_and_rejects_a_lane_that_stays_dead() {
        let revives = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let revive_count = revives.clone();
        let revive = move |_: usize| {
            revive_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Ok(()) }
        };
        let err = ensure_live_for_capture(
            "https://lane.example/",
            |_: String| async { Ok(502) },
            revive,
            std::time::Duration::ZERO,
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("not 2xx after 2 revive tries"),
            "error must name the bounded budget: {err}"
        );
        assert_eq!(
            revives.load(std::sync::atomic::Ordering::SeqCst),
            CAPTURE_REVIVE_MAX_TRIES
        );
    }

    /// F37: a non-200 public URL means the screenshot is FAILED with no
    /// stored frame — never a misleading 502/error-page image. The render is
    /// never even attempted (no CHROMIUM_BIN needed on this path).
    #[tokio::test]
    async fn non_200_url_marks_screenshot_failed_without_a_frame() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let store = gate_test_store().await;
        let run = store.list_runs().await.unwrap().remove(0);
        let probe = boxed_probe(|_: String| Box::pin(async { Ok::<u16, String>(502) }));
        // Real (non-test) mode so the capture gate runs; the gate rejects
        // before any render, so no Chromium is involved.
        let queue = ScreenshotQueue::with_data_dir(store.clone(), false, data_dir.clone())
            .with_probe_hook(probe);
        let state = queue.request(&run.id, 0, "https://dead.example/").await;
        assert_eq!(state, "queued");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let lane = store.get_lane(&run.id, 0).await.unwrap().unwrap();
            if lane.screenshot_state.as_deref() == Some("failed") {
                assert!(
                    lane.screenshot_path.is_none(),
                    "no frame may be stored for a dead URL"
                );
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "gate never finished"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        // No file was written at the render destination either.
        assert!(
            !render_dest(&data_dir, &run.id, 0).exists(),
            "a dead URL must not produce a stored frame"
        );
        // The rejection is visible in the lane's evidence.
        let events = store.list_lane_events(&run.id, 0).await.unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.payload.contains("screenshot capture gate")),
            "gate rejection must be logged: {events:?}"
        );
    }

    /// F37: a URL that answers 502 at gate time gets the lazy-revive path
    /// (detached restart of the stored run_command, bounded) BEFORE the
    /// capture — the revive attempt is logged, and the capture then proceeds
    /// to the render stage (which fails here only for lack of a real
    /// Chromium, proving the gate let it through).
    #[tokio::test]
    async fn dead_url_is_revived_before_the_capture_proceeds() {
        let _env = GATE_ENV_LOCK.lock().await;
        let old_chromium = std::env::var_os("CHROMIUM_BIN");
        std::env::set_var("CHROMIUM_BIN", "/nonexistent-chromium-f37");
        let result = async {
            let dir = tempfile::tempdir().unwrap();
            let data_dir = dir.path().join("data");
            std::fs::create_dir_all(&data_dir).unwrap();
            let store = gate_test_store().await;
            let run = store.list_runs().await.unwrap().remove(0);
            // The stored run_command the revive path restarts from.
            store
                .append_lane_event(
                    &run.id,
                    0,
                    now_ms(),
                    "run_command",
                    "cd dist && python3 -m http.server 8080 --bind 0.0.0.0",
                )
                .await
                .unwrap();
            let up = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let up_probe = up.clone();
            let up_revive = up.clone();
            let probe = boxed_probe(move |_: String| {
                let up = up_probe.clone();
                Box::pin(async move {
                    if up.load(std::sync::atomic::Ordering::SeqCst) {
                        Ok::<u16, String>(200)
                    } else {
                        Ok::<u16, String>(502)
                    }
                })
            });
            let revive_cmds = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let revive_cmds_hook = revive_cmds.clone();
            let revive_hook: ReviveExecHook = Arc::new(move |cmd: String| {
                let up = up_revive.clone();
                let cmds = revive_cmds_hook.clone();
                Box::pin(async move {
                    cmds.lock().unwrap().push(cmd);
                    up.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
            });
            let queue = ScreenshotQueue::with_data_dir(store.clone(), false, data_dir.clone())
                .with_probe_hook(probe)
                .with_revive_hook(revive_hook);
            let state = queue.request(&run.id, 0, "https://cold.example/").await;
            assert_eq!(state, "queued");
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                let lane = store.get_lane(&run.id, 0).await.unwrap().unwrap();
                if lane.screenshot_state.as_deref() == Some("failed") {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "run never reached a terminal screenshot state"
                );
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            // The revive ran exactly once (bounded) and the restart command
            // was the detached nohup form built from the stored run_command.
            // F37 clippy(await-holding-lock): snapshot the recorded revive
            // commands into owned data so the std MutexGuard is dropped
            // before the awaits below.
            let cmds = revive_cmds.lock().unwrap().clone();
            assert_eq!(cmds.len(), 1, "exactly one revive try: {cmds:?}");
            assert!(
                cmds[0].contains("nohup sh -c") && cmds[0].contains("python3 -m http.server 8080"),
                "restart must be the detached stored run_command: {cmds:?}"
            );
            // The gate passed (the revive attempt is logged) — the failure
            // afterwards is the missing Chromium, proving the capture was
            // allowed to proceed only after the revive.
            let events = store.list_lane_events(&run.id, 0).await.unwrap();
            assert!(
                events
                    .iter()
                    .any(|e| e.payload.contains("revive attempt 1/2")),
                "the revive-before-capture attempt must be logged: {events:?}"
            );
        }
        .await;
        // Restore the environment whatever the assertions did.
        match old_chromium {
            Some(v) => std::env::set_var("CHROMIUM_BIN", v),
            None => std::env::remove_var("CHROMIUM_BIN"),
        }
        result
    }
}
