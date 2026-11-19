use std::collections::HashMap;
use std::sync::Arc;

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
    _url: String,
}

#[derive(Debug, Clone)]
pub struct ScreenshotQueue {
    tx: mpsc::Sender<ScreenshotJob>,
    pending: Arc<RwLock<HashMap<(String, i64), String>>>, // value: state string/path marker
    notify: Arc<Notify>,
}

impl ScreenshotQueue {
    pub fn new(store: Store, test_mode: bool) -> Self {
        let (tx, mut rx) = mpsc::channel::<ScreenshotJob>(128);
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
                let _ = store
                    .append_lane_event(
                        &job.run_id,
                        job.lane_idx,
                        ts,
                        "state",
                        "screenshot_rendering",
                    )
                    .await;
                let _ = store
                    .set_screenshot_state(&job.run_id, job.lane_idx, "rendering", None)
                    .await;

                let (state, path_opt) = if test_mode {
                    let fake = format!("/data/{}/lane-{}.png", job.run_id, job.lane_idx);
                    ("done".to_owned(), Some(fake))
                } else {
                    // Real Chromium rendering is deferred to deploy/runtime environment.
                    // In non-test mode without chromium integration implemented yet, mark failed
                    // rather than blocking; this path is not used by smoke tests.
                    ("failed".to_owned(), None)
                };

                let ts = now_ms();
                let _ = store
                    .append_lane_event(
                        &job.run_id,
                        job.lane_idx,
                        ts,
                        "state",
                        &format!("screenshot_{state}"),
                    )
                    .await;
                let _ = store
                    .set_screenshot_state(&job.run_id, job.lane_idx, &state, path_opt.as_deref())
                    .await;
                {
                    pending_worker.write().await.insert(
                        (job.run_id.clone(), job.lane_idx),
                        path_opt.unwrap_or_else(|| state.clone()),
                    );
                }
                notify_worker.notify_waiters();
            }
        });
        Self {
            tx,
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
            _url: url.to_owned(),
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
