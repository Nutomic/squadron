use serde::{Deserialize, Serialize};
use serde_json::Value;
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

/// Wave A: the judge's brief-fulfillment verdict for the winning lane, as
/// rendered on the run page's verdict card. `state` is one of
/// "fulfilled" | "unfulfilled" | "unverifiable" (verbatim from the judge
/// schema); `evidence` is judge-written text (untrusted DATA — the island
/// renders it escaped).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BriefFulfillmentView {
    pub state: String,
    pub evidence: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunSnapshot {
    pub id: String,
    pub status: String,
    pub judge_verdict: Option<String>,
    pub winner_lane: Option<i64>,
    /// Wave A: None for runs judged before the field existed (or when the
    /// stored JSON failed to parse) — the island omits the line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judge_brief_fulfillment: Option<BriefFulfillmentView>,
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
    /// Wave D progress probes: the newest parsed probe report for the lane
    /// (None until the first probe lands; probes are best-effort telemetry).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_pct: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// F25: the case's user-visible impl_prompt (brief + distinct angle, no
    /// boilerplate constraints — those are appended at execution time), parsed
    /// from the persisted variant_json. None for template-fallback lanes
    /// (classic variant shape) or unparsable legacy JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impl_prompt: Option<String>,
}

pub fn run_snapshot(run: &Run) -> RunSnapshot {
    RunSnapshot {
        id: run.id.clone(),
        status: run.status.clone(),
        judge_verdict: run.judge_verdict.clone(),
        winner_lane: run.winner_lane,
        judge_brief_fulfillment: None,
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
    let impl_prompt = serde_json::from_str::<Value>(&lane.variant_json)
        .ok()
        .and_then(|v| {
            v.get("impl_prompt")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
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
        progress_pct: None,
        stage: None,
        impl_prompt,
    }
}

/// Wave D: parse one `progress` lane-event payload
/// ({"progress_pct":<0-100>,"stage":"..."}). Malformed payloads are ignored.
fn parse_progress_payload(payload: &str) -> Option<(u8, String)> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let pct = v.get("progress_pct")?.as_u64()?;
    if pct > 100 {
        return None;
    }
    let stage = v.get("stage")?.as_str()?.trim();
    if stage.is_empty() {
        return None;
    }
    Some((pct as u8, stage.to_owned()))
}

fn tokei_totals(json: Option<&str>) -> (Option<i64>, Option<i64>) {
    // #16: single canonical tokei parser shared with the orchestrator.
    crate::artifacts::parse_tokei_totals(json.unwrap_or_default())
}

/// Rebuild the full snapshot from the store (late-subscriber fallback path).
pub async fn build_snapshot(store: &Store, run_id: &str) -> Option<Snapshot> {
    let run = store.get_run(run_id).await.ok()??;
    let lanes = store.list_lanes(run_id).await.ok()?;
    let mut run_snap = run_snapshot(&run);
    // Wave A: hydrate the judge's brief-fulfillment verdict (None for runs
    // judged before the column existed or with unparsable JSON).
    if let Some(json) = store
        .get_judge_brief_fulfillment(run_id)
        .await
        .ok()
        .flatten()
    {
        run_snap.judge_brief_fulfillment = serde_json::from_str::<BriefFulfillmentView>(&json).ok();
    }
    let mut lane_snaps = Vec::with_capacity(lanes.len());
    for lane in lanes {
        let series = cost_series(store, &run.id, lane.idx).await;
        let mut snap = lane_snapshot(&run, &lane, series);
        // Wave D: hydrate the newest progress-probe report, if any.
        if let Ok(Some(event)) = store
            .latest_lane_event_of_kind(&run.id, lane.idx, "progress")
            .await
        {
            if let Some((pct, stage)) = parse_progress_payload(&event.payload) {
                snap.progress_pct = Some(pct);
                snap.stage = Some(stage);
            }
        }
        lane_snaps.push(snap);
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

    /// Wave A: a run judged after the brief_fulfillment column existed.
    fn judged_run_with_fulfillment() -> Run {
        Run {
            judge_verdict: Some("Bold and playable. It wins.".to_owned()),
            winner_lane: Some(1),
            ..run()
        }
    }

    #[test]
    fn run_snapshot_hydrates_nothing_without_a_store() {
        // run_snapshot alone has no brief fulfillment (hydrated in
        // build_snapshot from the store).
        assert_eq!(
            run_snapshot(&judged_run_with_fulfillment()).judge_brief_fulfillment,
            None
        );
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

    /// F25: the lane card prompt is parsed out of the persisted variant_json;
    /// template-fallback lanes (classic variant shape) carry no impl_prompt.
    #[test]
    fn lane_snapshot_carries_the_case_impl_prompt() {
        let mut l = lane();
        l.variant_json =
            r#"{"label":"green","impl_prompt":"green arcade landing page for the brief"}"#
                .to_owned();
        let snap = lane_snapshot(&run(), &l, None);
        assert_eq!(
            snap.impl_prompt.as_deref(),
            Some("green arcade landing page for the brief")
        );
        // Classic variant shape: no impl_prompt key.
        let mut v = lane();
        v.variant_json = r#"{"angle":"a","stack":"s","tone":"t"}"#.to_owned();
        assert_eq!(lane_snapshot(&run(), &v, None).impl_prompt, None);
        // Unparsable JSON is tolerated (None, never a panic).
        let mut u = lane();
        u.variant_json = "not json".to_owned();
        assert_eq!(lane_snapshot(&run(), &u, None).impl_prompt, None);
    }

    /// Wave D: progress payloads parse strictly; garbage is ignored.
    #[test]
    fn parse_progress_payload_strict() {
        assert_eq!(
            parse_progress_payload(r#"{"progress_pct":42,"stage":"building ui"}"#),
            Some((42, "building ui".to_owned()))
        );
        assert_eq!(parse_progress_payload("no json"), None);
        assert_eq!(
            parse_progress_payload(r#"{"progress_pct":101,"stage":"s"}"#),
            None
        );
        assert_eq!(parse_progress_payload(r#"{"stage":"s"}"#), None);
        assert_eq!(
            parse_progress_payload(r#"{"progress_pct":42,"stage":""}"#),
            None
        );
    }
}
