use serde::{Deserialize, Serialize};
use store::{Lane, Run};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunSnapshot {
    pub run: RunView,
    pub lanes: Vec<LaneView>,
    pub ts: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunView {
    pub id: String,
    pub status: String,
    pub judge_verdict: Option<String>,
    pub winner_lane: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LaneView {
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
}

pub fn lane_loc(tokei_json: &Option<String>) -> Option<i64> {
    let s = tokei_json.as_deref()?;
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    // Try common shapes: {"loc":N} or {"Total":{"code":N}} or number
    if let Some(n) = v.get("loc").and_then(|x| x.as_i64()) {
        return Some(n);
    }
    if let Some(n) = v.get("code").and_then(|x| x.as_i64()) {
        return Some(n);
    }
    if let Some(total) = v.get("Total").or_else(|| v.get("total")) {
        if let Some(n) = total.get("code").and_then(|x| x.as_i64()) {
            return Some(n);
        }
        if let Some(n) = total.get("loc").and_then(|x| x.as_i64()) {
            return Some(n);
        }
    }
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    None
}

pub fn snapshot_for(run: &Run, lanes: &[Lane], ts: i64) -> RunSnapshot {
    RunSnapshot {
        run: RunView {
            id: run.id.clone(),
            status: run.status.clone(),
            judge_verdict: run.judge_verdict.clone(),
            winner_lane: run.winner_lane,
        },
        lanes: lanes
            .iter()
            .map(|l| LaneView {
                idx: l.idx,
                state: l.state.clone(),
                model: l.impl_model.clone(),
                impl_url: l.impl_url.clone(),
                cost_usd: l.cost_usd,
                build_ms: match (l.impl_started_at, l.val_deadline, l.impl_deadline) {
                    // best-effort: not specified precisely; keep null unless we stored duration
                    _ => None,
                },
                files: l.impl_files,
                loc: lane_loc(&l.tokei_json),
                screenshot_state: l
                    .screenshot_state
                    .clone()
                    .unwrap_or_else(|| "pending".to_owned()),
                val_verdict: l.val_verdict.clone(),
                verdict_card: if run.winner_lane == Some(l.idx) {
                    run.judge_verdict.clone()
                } else {
                    None
                },
            })
            .collect(),
        ts,
    }
}
