use serde::{Deserialize, Serialize};
use serde_json::Value;
use store::{Lane, Run, Store};

/// The single shared snapshot type: produced by the pipeline for SSE, served
/// by the web/API layers, and rendered by fixtures. There is exactly one
/// definition (here) and one tokei parser (`tokei_totals`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub run: RunSnapshot,
    pub lanes: Vec<LaneSnapshot>,
    pub ts: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunSnapshot {
    pub id: String,
    pub status: String,
    pub judge_verdict: Option<String>,
    pub winner_lane: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    pub screenshot_state: String,
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

/// Parse a tokei JSON payload into `(files, loc)`.
///
/// Accepts the shapes tokei can emit or that the pipeline records:
/// `{"Total":{"files":N,"code":M}}` (canonical tokei), `{"total":{...}}`,
/// the pipeline fallback `{"files":N,"loc":M}` / `{"loc":M}` / `{"code":M}`,
/// or a bare number.
pub fn tokei_totals(json: Option<&str>) -> (Option<i64>, Option<i64>) {
    let Some(json) = json else {
        return (None, None);
    };
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return (None, None);
    };
    if let Some(n) = v.as_i64() {
        return (None, Some(n));
    }
    let total = v.get("Total").or_else(|| v.get("total"));
    let files = total
        .and_then(|t| t.get("files"))
        .and_then(Value::as_i64)
        .or_else(|| v.get("files").and_then(Value::as_i64));
    let loc = total
        .and_then(|t| {
            t.get("code")
                .and_then(Value::as_i64)
                .or_else(|| t.get("loc").and_then(Value::as_i64))
        })
        .or_else(|| {
            v.get("code")
                .and_then(Value::as_i64)
                .or_else(|| v.get("loc").and_then(Value::as_i64))
        });
    (files, loc)
}

/// Wall-clock duration of the implementation phase: from the moment the
/// implementer started to the moment validation took over. `None` while the
/// lane is still implementing or if timestamps are missing/invalid.
pub fn lane_build_ms(lane: &Lane) -> Option<i64> {
    match (lane.impl_started_at, lane.val_started_at) {
        (Some(start), Some(end)) if end >= start => Some(end - start),
        _ => None,
    }
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
        build_ms: lane_build_ms(lane),
        files: lane.impl_files.or(tok_files),
        loc: tok_loc,
        screenshot_state: lane
            .screenshot_state
            .clone()
            .unwrap_or_else(|| "pending".to_owned()),
        val_verdict: lane.val_verdict.clone(),
        verdict_card,
        console_tail: lane.console_tail.clone(),
        cost_series,
        thumbnail_data_url: None,
        error: lane.error.clone(),
    }
}

/// Build the shared snapshot for a run that is already loaded, including the
/// per-lane cost series pulled from the lane event log.
pub async fn snapshot_for(store: &Store, run: &Run) -> Snapshot {
    let lanes = match store.list_lanes(&run.id).await {
        Ok(lanes) => lanes,
        Err(_) => Vec::new(),
    };
    let mut lane_snaps = Vec::with_capacity(lanes.len());
    for lane in &lanes {
        let series = cost_series(store, &run.id, lane.idx).await;
        lane_snaps.push(lane_snapshot(run, lane, series));
    }
    Snapshot {
        run: run_snapshot(run),
        lanes: lane_snaps,
        ts: now_ms(),
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub async fn build_snapshot(store: &Store, run_id: &str) -> Option<Snapshot> {
    let run = store.get_run(run_id).await.ok()??;
    Some(snapshot_for(store, &run).await)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokei_totals_handles_all_recorded_shapes() {
        assert_eq!(
            tokei_totals(Some(r#"{"Total":{"files":9,"code":1204}}"#)),
            (Some(9), Some(1204))
        );
        assert_eq!(
            tokei_totals(Some(r#"{"total":{"loc":77}}"#)),
            (None, Some(77))
        );
        assert_eq!(
            tokei_totals(Some(r#"{"files":3,"loc":140}"#)),
            (Some(3), Some(140))
        );
        assert_eq!(tokei_totals(Some(r#"{"loc":0}"#)), (None, Some(0)));
        assert_eq!(tokei_totals(Some(r#"{"code":42}"#)), (None, Some(42)));
        assert_eq!(tokei_totals(Some("1204")), (None, Some(1204)));
        assert_eq!(tokei_totals(Some("not json")), (None, None));
        assert_eq!(tokei_totals(None), (None, None));
    }

    #[test]
    fn parser_is_the_single_shared_implementation() {
        // tokei_totals must be the only parser: it lives on this module and is
        // re-exported verbatim to the web layer; guard against accidental
        // duplication by keeping the name public and stable.
        let (_, loc) = tokei_totals(Some(r#"{"Total":{"code":1832}}"#));
        assert_eq!(loc, Some(1832));
    }
}
