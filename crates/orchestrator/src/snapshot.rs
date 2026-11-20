use serde::Serialize;
use store::{now_ms, Lane, Run, Store};

/// Canonical run snapshot serialized through the SSE hub. The hub payload is
/// the live source for subscribers; the DB rebuild (`build_snapshot`) is only
/// the fallback for late subscribers that missed the broadcast.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub run: RunSnapshot,
    pub lanes: Vec<LaneSnapshot>,
    pub ts: i64,
}

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

pub fn run_snapshot(run: &Run) -> RunSnapshot {
    RunSnapshot {
        id: run.id.clone(),
        status: run.status.clone(),
        judge_verdict: run.judge_verdict.clone(),
        winner_lane: run.winner_lane,
    }
}

pub fn lane_snapshot(run: &Run, lane: &Lane, cost_series: Option<Vec<f64>>) -> LaneSnapshot {
    // Build time: implementer session length (impl start -> validation start).
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
    // #16: single canonical tokei parser shared with the orchestrator.
    crate::artifacts::parse_tokei_totals(json.unwrap_or_default())
}

/// Rebuild the full snapshot from the store (late-subscriber fallback path).
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

#[cfg(test)]
mod tests {
    use super::*;

    fn lane() -> Lane {
        Lane {
            run_id: "r".to_owned(),
            idx: 0,
            variant_json: "{}".to_owned(),
            sprite_name: None,
            impl_url: None,
            state: "validated".to_owned(),
            impl_model: None,
            impl_summary: None,
            impl_files: None,
            impl_started_at: None,
            impl_deadline: None,
            val_model: None,
            val_verdict: None,
            val_summary: None,
            val_quality: None,
            val_started_at: None,
            val_deadline: None,
            val_report_json: None,
            tarball_path: None,
            tokei_json: None,
            screenshot_path: None,
            screenshot_state: None,
            console_tail: None,
            cost_usd: None,
            error: None,
        }
    }

    fn run() -> Run {
        Run {
            id: "r".to_owned(),
            brief: "b".to_owned(),
            status: "judged".to_owned(),
            n_lanes: 3,
            created_at: 0,
            judge_verdict: None,
            winner_lane: None,
            total_cost_usd: None,
            finished_at: None,
        }
    }

    /// #7: build_ms is computed from the implementer session, not hardcoded None.
    #[test]
    fn lane_snapshot_computes_build_ms_from_impl_session() {
        let mut l = lane();
        l.impl_started_at = Some(1_000);
        l.val_started_at = Some(64_000);
        let snap = lane_snapshot(&run(), &l, None);
        assert_eq!(snap.build_ms, Some(63_000));
        // No session timestamps -> no build time.
        let mut l2 = lane();
        l2.impl_started_at = None;
        assert_eq!(lane_snapshot(&run(), &l2, None).build_ms, None);
        // Non-monotonic timestamps are not a build time.
        let mut l3 = lane();
        l3.impl_started_at = Some(2_000);
        l3.val_started_at = Some(1_000);
        assert_eq!(lane_snapshot(&run(), &l3, None).build_ms, None);
    }
}
