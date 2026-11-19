use serde::Serialize;
use serde_json::Value;
use store::{Lane, Run, Store};

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
        screenshot_state: Some(
            lane.screenshot_state
                .clone()
                .unwrap_or_else(|| "pending".to_owned()),
        ),
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

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub async fn build_snapshot(store: &Store, run_id: &str) -> Option<Snapshot> {
    let run = store.get_run(run_id).await.ok()??;
    let lanes = store.list_lanes(run_id).await.ok()?;
    let run_snap = run_snapshot(&run);
    let mut lane_snaps = Vec::with_capacity(lanes.len());
    for lane in lanes {
        let series = cost_series(store, &run.id, lane.idx).await;
        lane_snaps.push(lane_snapshot(&run, &lane, series));
    }
    Some(Snapshot {
        run: run_snap,
        lanes: lane_snaps,
        ts: now_ms(),
    })
}

async fn cost_series(store: &Store, run_id: &str, idx: i64) -> Option<Vec<f64>> {
    let events = store.list_lane_events(run_id, idx).await.ok()?;
    let mut series: Vec<f64> = Vec::new();
    let mut total = 0.0f64;
    for ev in events {
        if ev.kind != "cost" {
            continue;
        }
        if let Ok(delta) = serde_json::from_str::<f64>(&ev.payload) {
            total += delta;
            series.push(total);
        }
    }
    if series.is_empty() {
        None
    } else {
        Some(series)
    }
}
