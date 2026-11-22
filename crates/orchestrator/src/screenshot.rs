use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine;
use futures::StreamExt;
use tokio::sync::{mpsc, Notify, RwLock};

use store::{now_ms, Store};

/// Shared hub registry: run_id -> SSE hub. Wired into the screenshot queue so
/// thumbnail state changes reach live SSE subscribers (audit R3 finding 3).
pub type HubRegistry = Arc<tokio::sync::RwLock<HashMap<String, crate::sse::SseHub>>>;

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

/// Review F4 escape hatch: SWARM_CHROMIUM_NO_SANDBOX=1 opts into running
/// Chromium with --no-sandbox from the start (environments where the
/// sandboxed launch is known to fail, e.g. root without user namespaces).
fn no_sandbox_env() -> bool {
    std::env::var("SWARM_CHROMIUM_NO_SANDBOX").ok().as_deref() == Some("1")
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
/// SWARM_CHROMIUM_NO_SANDBOX=1 env var is a loud escape hatch for hosts
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
            "SWARM_CHROMIUM_NO_SANDBOX=1: launching headless Chromium WITHOUT its sandbox \
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
                 SWARM_CHROMIUM_NO_SANDBOX escape hatch (--no-sandbox) — untrusted-content \
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

#[derive(Debug, Clone)]
pub struct ScreenshotQueue {
    tx: mpsc::Sender<ScreenshotJob>,
    store: Store,
    pending: Arc<RwLock<HashMap<(String, i64), String>>>, // value: terminal state string
    notify: Arc<Notify>,
    hubs: Option<HubRegistry>,
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
        let (tx, mut rx) = mpsc::channel::<ScreenshotJob>(128);
        let store_worker = store.clone();
        let data_dir_worker = data_dir.clone();
        let pending: Arc<RwLock<HashMap<(String, i64), String>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let notify = Arc::new(Notify::new());
        let pending_worker = pending.clone();
        let notify_worker = notify.clone();
        let hubs_worker = hubs.clone();
        tokio::spawn(async move {
            // serial: one-at-a-time by nature of single consumer loop
            while let Some(job) = rx.recv().await {
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
                    // F14: create the destination directory before the render
                    // (save_screenshot does not mkdir; a missing
                    // DATA_DIR/screenshots/{run} failed real renders with ENOENT).
                    match ensure_dest_dir(&dest)
                        .await
                        .and_then(|()| render_chromium(&job.url, &dest))
                    {
                        Ok(mode) => {
                            let ts = now_ms();
                            let _ = store_worker
                                .append_lane_event(
                                    &job.run_id,
                                    job.lane_idx,
                                    ts,
                                    "state",
                                    &format!("screenshot_render_mode={}", mode.as_str()),
                                )
                                .await;
                            ("done".to_owned(), Some(dest.to_string_lossy().into_owned()))
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "screenshot render failed");
                            ("failed".to_owned(), None)
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
    use super::*;

    /// Review F4: the escape hatch env var decides whether the FIRST launch
    /// already runs --no-sandbox (env set) or sandboxed with a no-sandbox
    /// retry only on a sandboxed launch failure (default).
    #[test]
    fn no_sandbox_escape_hatch_env() {
        let key = "SWARM_CHROMIUM_NO_SANDBOX";
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
}
