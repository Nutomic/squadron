use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::{mpsc, Notify, RwLock};

use crate::config::now_ms;
use store::Store;

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

fn render_dest(run_id: &str, lane_idx: i64) -> PathBuf {
    PathBuf::from("/data")
        .join("screenshots")
        .join(run_id)
        .join(format!("lane-{lane_idx}.png"))
}

/// Render a lane's live URL to a PNG with headless Chromium (chromiumoxide).
async fn render_chromium(url: &str, dest: &std::path::Path) -> Result<(), String> {
    let bin = std::env::var_os("CHROMIUM_BIN")
        .map(PathBuf::from)
        .ok_or_else(|| "CHROMIUM_BIN not set; no chromium available".to_owned())?;
    let config = chromiumoxide::BrowserConfig::builder()
        .chrome_executable(bin)
        .no_sandbox()
        .window_size(1280, 800)
        .launch_timeout(std::time::Duration::from_secs(30))
        .request_timeout(std::time::Duration::from_secs(45))
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

#[derive(Debug, Clone)]
pub struct ScreenshotQueue {
    tx: mpsc::Sender<ScreenshotJob>,
    store: Store,
    pending: Arc<RwLock<HashMap<(String, i64), String>>>, // value: terminal state string
    notify: Arc<Notify>,
}

impl ScreenshotQueue {
    pub fn new(store: Store, test_mode: bool) -> Self {
        let (tx, mut rx) = mpsc::channel::<ScreenshotJob>(128);
        let store_worker = store.clone();
        let pending: Arc<RwLock<HashMap<(String, i64), String>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let notify = Arc::new(Notify::new());
        let pending_worker = pending.clone();
        let notify_worker = notify.clone();
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

                let (state, path_opt) = if test_mode {
                    let fake = format!("/data/{}/lane-{}.png", job.run_id, job.lane_idx);
                    ("done".to_owned(), Some(fake))
                } else {
                    // Real Chromium rendering: serial worker (this loop is the
                    // only consumer). Requires CHROMIUM_BIN; without it the
                    // screenshot fails honestly instead of blocking.
                    let dest = render_dest(&job.run_id, job.lane_idx);
                    match render_chromium(&job.url, &dest).await {
                        Ok(()) => ("done".to_owned(), Some(dest.to_string_lossy().into_owned())),
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
                notify_worker.notify_waiters();
            }
        });
        Self {
            tx,
            store,
            pending,
            notify,
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
}
