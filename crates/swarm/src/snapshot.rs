use serde::Serialize;
use serde_json::Value;
use store::{Lane, Run};

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunSnapshot {
    pub id: String,
    pub status: String,
    pub judge_verdict: Option<String>,
    pub winner_lane: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LaneSnapshot {
    pub idx: i64,
    pub state: String,
    pub model: Option<String>,
    pub impl_url: Option<String>,
    pub cost_usd: Option<f64>,
    pub build_ms: Option<i64>,
    pub files: Option<i64>,
    pub loc: Option<i64>,
    pub screenshot_state: Option<String>,
    pub val_verdict: Option<String>,
    pub verdict_card: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub console_tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_series: Option<Vec<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_data_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Snapshot {
    pub run: RunSnapshot,
    pub lanes: Vec<LaneSnapshot>,
    pub ts: i64,
}

pub fn run_snapshot(run: &Run) -> RunSnapshot {
    RunSnapshot {
        id: run.id.clone(),
        status: run.status.clone(),
        judge_verdict: run.judge_verdict.clone(),
        winner_lane: run.winner_lane,
    }
}

pub fn lane_snapshot(run: &Run, lane: &Lane, cost_series: Option<Vec<f64>>) -> LaneSnapshot {
    let build_ms = match (lane.impl_started_at, lane.val_started_at) {
        (Some(start), Some(end)) if end >= start => Some(end - start),
        _ => None,
    };
    let (tok_files, tok_loc) = tokei_totals(lane.tokei_json.as_deref());
    let verdict_card = match (run.winner_lane, run.judge_verdict.as_deref()) {
        (Some(w), Some(v)) if w == lane.idx => Some(v.to_owned()),
        _ => None,
    };
    LaneSnapshot {
        idx: lane.idx,
        state: lane.state.clone(),
        model: lane.impl_model.clone(),
        impl_url: lane.impl_url.clone(),
        cost_usd: lane.cost_usd,
        build_ms,
        files: lane.impl_files.or(tok_files),
        loc: tok_loc,
        screenshot_state: lane.screenshot_state.clone(),
        val_verdict: lane.val_verdict.clone(),
        verdict_card,
        console_tail: lane.console_tail.clone(),
        cost_series,
        thumbnail_data_url: None,
        error: lane.error.clone(),
    }
}

fn tokei_totals(json: Option<&str>) -> (Option<i64>, Option<i64>) {
    let Some(json) = json else {
        return (None, None);
    };
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return (None, None);
    };
    let total = v.get("Total");
    (
        total.and_then(|t| t.get("files")).and_then(Value::as_i64),
        total.and_then(|t| t.get("code")).and_then(Value::as_i64),
    )
}
