mod common;

use std::time::Duration;

use common::{canned_impl, canned_val, harness, sh, wait_lanes_settled, ENV_LOCK};
use orchestrator::{start_run, RunRequest};

/// Finding #22: the implementer deadline must actually be enforced. The mock
/// exec sleeps far past `impl_deadline`; the lane must end `failed` with an
/// error naming the deadline, and the run must not be judged.
#[tokio::test(flavor = "multi_thread")]
async fn impl_deadline_exceeded_fails_lane_and_run() {
    let _guard = ENV_LOCK.lock().await;
    let (ctx, store, _dir) = common::setup_ctx(|cfg| {
        cfg.impl_deadline = Duration::from_millis(700);
        cfg.val_deadline = Duration::from_secs(30);
    })
    .await;

    // exec sleeps 3s; deadline is 0.7s
    let impl_cmd = format!("sleep 3; {}", sh(&canned_impl()));
    let req = RunRequest {
        brief: "deadline impl test".to_owned(),
        n_lanes: 3,
        harness: harness(&impl_cmd, &sh(&canned_val())),
    };
    let run = start_run(ctx.clone(), req).await.unwrap();
    let lanes = wait_lanes_settled(&store, &run.id).await;

    assert_eq!(lanes.len(), 3);
    for lane in &lanes {
        assert_eq!(
            lane.state, "failed",
            "lane {} state={}",
            lane.idx, lane.state
        );
        let err = lane.error.clone().unwrap_or_default();
        assert!(
            err.contains("implementer:") && err.contains("deadline exceeded"),
            "lane {} error={err:?}",
            lane.idx
        );
        // nothing half-finished may linger as a summary
        assert!(lane.impl_summary.is_none());
        assert!(lane.val_verdict.is_none());
    }

    let final_run = store.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(final_run.status, "failed");
    assert!(final_run.judge_verdict.is_none());
    assert!(final_run.winner_lane.is_none());

    common::clear_mock_env();
}

/// Finding #22 (validator side): the validator deadline is enforced too; a
/// validator that sleeps past `val_deadline` fails the lane even though the
/// implementer succeeded.
#[tokio::test(flavor = "multi_thread")]
async fn validator_deadline_exceeded_fails_lane_and_run() {
    let _guard = ENV_LOCK.lock().await;
    let (ctx, store, _dir) = common::setup_ctx(|cfg| {
        cfg.impl_deadline = Duration::from_secs(30);
        cfg.val_deadline = Duration::from_millis(700);
    })
    .await;

    let val_cmd = format!("sleep 3; {}", sh(&canned_val()));
    let req = RunRequest {
        brief: "deadline val test".to_owned(),
        n_lanes: 3,
        harness: harness(&sh(&canned_impl()), &val_cmd),
    };
    let run = start_run(ctx.clone(), req).await.unwrap();
    let lanes = wait_lanes_settled(&store, &run.id).await;

    for lane in &lanes {
        assert_eq!(
            lane.state, "failed",
            "lane {} state={}",
            lane.idx, lane.state
        );
        let err = lane.error.clone().unwrap_or_default();
        assert!(
            err.contains("validator:") && err.contains("deadline exceeded"),
            "lane {} error={err:?}",
            lane.idx
        );
        assert!(lane.val_verdict.is_none(), "no verdict may be recorded");
    }

    let final_run = store.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(final_run.status, "failed");
    assert!(final_run.judge_verdict.is_none());

    common::clear_mock_env();
}
