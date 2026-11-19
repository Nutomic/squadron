use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub brief: String,
    pub status: String,
    pub n_lanes: i64,
    pub created_at: i64,
    pub judge_verdict: Option<String>,
    pub winner_lane: Option<i64>,
    pub total_cost_usd: Option<f64>,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lane {
    pub run_id: String,
    pub idx: i64,
    pub variant_json: String,
    pub sprite_name: Option<String>,
    pub impl_url: Option<String>,
    pub state: String,
    pub impl_model: Option<String>,
    pub impl_summary: Option<String>,
    pub impl_files: Option<i64>,
    pub impl_started_at: Option<i64>,
    pub impl_deadline: Option<i64>,
    pub val_model: Option<String>,
    pub val_verdict: Option<String>,
    pub val_summary: Option<String>,
    pub val_quality: Option<f64>,
    pub val_started_at: Option<i64>,
    pub val_deadline: Option<i64>,
    pub tarball_path: Option<String>,
    pub tokei_json: Option<String>,
    pub screenshot_path: Option<String>,
    pub screenshot_state: Option<String>,
    pub console_tail: Option<String>,
    pub cost_usd: Option<f64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneEvent {
    pub run_id: String,
    pub lane_idx: i64,
    pub ts: i64,
    pub kind: String,
    pub payload: String,
}
