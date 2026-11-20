mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::Json;
use axum::routing::post;
use axum::Router;
use common::{canned_impl, canned_val, harness, sh, wait_lanes_settled, ENV_LOCK};
use llm_gateway::MachineLlmClient;
use orchestrator::judge::run_judge;
use orchestrator::{start_run, PipelineCtx, RunRequest};
use prompts::{LaneJudgeReport, ValidatorOutput, Verdict};
use serde_json::{json, Value};

fn completion(content: &str) -> Value {
    json!({
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}]
    })
}

async fn serve(router: Router) -> u16 {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    port
}

/// Finding #24: with only one validated lane there is no fair comparison, so
/// the judge phase must be skipped and the run failed.
#[tokio::test(flavor = "multi_thread")]
async fn single_validated_lane_skips_judge_and_fails_run() {
    let _guard = ENV_LOCK.lock().await;
    let (ctx, store, _dir) = common::setup_ctx(|cfg| {
        cfg.n_lanes = 3;
    })
    .await;

    // lanes 0/1 emit schema-invalid JSON, lane 2 succeeds (workdir contains
    // "lane-2"), leaving exactly one validated lane
    let impl_cmd = "case \"$PWD\" in \
        *lane-2*) echo '{\"summary\":\"Implemented arcade page\",\"files\":3,\"run_command\":\"python3 -m http.server 8080\",\"done\":true,\"blockers\":[]}' ;; \
        *) echo 'JUDGE-GATE-INVALID-JSON' ;; esac";
    let req = RunRequest {
        brief: "one validated lane".to_owned(),
        n_lanes: 3,
        harness: harness(impl_cmd, &sh(&canned_val())),
    };
    let run = start_run(ctx.clone(), req).await.unwrap();
    let lanes = wait_lanes_settled(&store, &run.id).await;

    let validated: Vec<_> = lanes.iter().filter(|l| l.state == "validated").collect();
    assert_eq!(validated.len(), 1, "exactly one lane validates");
    assert!(validated[0].idx == 2);

    let final_run = store.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(final_run.status, "failed");
    assert!(
        final_run.judge_verdict.is_none(),
        "no verdict without >=2 validated lanes"
    );
    assert!(
        final_run.winner_lane.is_none(),
        "no winner without >=2 validated lanes"
    );

    common::clear_mock_env();
}

/// Finding #24: a judge that crowns a winner outside its own ranking must be
/// rejected by the gate (direct `run_judge` test).
#[tokio::test(flavor = "multi_thread")]
async fn judge_winner_outside_ranking_is_rejected() {
    let content = json!({
        "ranking": [0, 1],
        "winner": 5,
        "verdict_card": "Lane five wins. It is best.",
        "reasons": "clear winner"
    })
    .to_string();
    let port = serve(Router::new().route(
        "/chat/completions",
        post(move || async move { Json(completion(&content)) }),
    ))
    .await;
    let client = MachineLlmClient::new(
        "test-key",
        format!("http://127.0.0.1:{port}/chat/completions"),
    )
    .with_timeout(Duration::from_secs(10));

    let reports = vec![
        LaneJudgeReport::new(0, sample_report()),
        LaneJudgeReport::new(1, sample_report()),
    ];
    let extracts = vec!["<extract 0>".to_owned(), "<extract 1>".to_owned()];
    let err = run_judge(&client, &reports, &extracts, Duration::from_secs(10))
        .await
        .expect_err("judge output must be rejected");
    assert!(
        err.contains("winner must appear in ranking"),
        "unexpected error: {err}"
    );
}

/// Finding #24: the ranking must be a permutation of the validated lane
/// indices, not some arbitrary set.
#[tokio::test(flavor = "multi_thread")]
async fn judge_ranking_not_a_permutation_is_rejected() {
    let content = json!({
        "ranking": [0, 5],
        "winner": 0,
        "verdict_card": "Lane zero wins. Clear best.",
        "reasons": "best layout"
    })
    .to_string();
    let port = serve(Router::new().route(
        "/chat/completions",
        post(move || async move { Json(completion(&content)) }),
    ))
    .await;
    let client = MachineLlmClient::new(
        "test-key",
        format!("http://127.0.0.1:{port}/chat/completions"),
    )
    .with_timeout(Duration::from_secs(10));

    let reports = vec![
        LaneJudgeReport::new(0, sample_report()),
        LaneJudgeReport::new(1, sample_report()),
    ];
    let extracts = vec!["<extract 0>".to_owned(), "<extract 1>".to_owned()];
    let err = run_judge(&client, &reports, &extracts, Duration::from_secs(10))
        .await
        .expect_err("judge output must be rejected");
    assert!(
        err.contains("ranking must be a permutation of lane indices"),
        "unexpected error: {err}"
    );
}

/// Finding #24 (pipeline level): a full run where every lane validates but the
/// judge emits a winner outside the ranking ends with the run failed and no
/// verdict persisted.
#[tokio::test(flavor = "multi_thread")]
async fn bad_judge_output_fails_run_end_to_end() {
    let _guard = ENV_LOCK.lock().await;
    let content = json!({
        "ranking": [0, 1, 2],
        "winner": 9,
        "verdict_card": "Lane nine sweeps. Undisputed.",
        "reasons": "dominant"
    })
    .to_string();
    let port = serve(Router::new().route(
        "/chat/completions",
        post(move || async move { Json(completion(&content)) }),
    ))
    .await;
    let client = MachineLlmClient::new(
        "test-key",
        format!("http://127.0.0.1:{port}/chat/completions"),
    )
    .with_timeout(Duration::from_secs(10));

    let (ctx, store, _dir) = common::setup_ctx(|cfg| {
        cfg.n_lanes = 3;
    })
    .await;
    let ctx = PipelineCtx {
        machine_llm: Some(Arc::new(client)),
        ..(*ctx).clone()
    };
    let ctx = Arc::new(ctx);

    let req = RunRequest {
        brief: "bad judge end to end".to_owned(),
        n_lanes: 3,
        harness: harness(&sh(&canned_impl()), &sh(&canned_val())),
    };
    let run = start_run(ctx.clone(), req).await.unwrap();
    let lanes = wait_lanes_settled(&store, &run.id).await;

    assert!(
        lanes.iter().all(|l| l.state == "validated"),
        "all lanes should validate"
    );
    let final_run = store.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(final_run.status, "failed");
    assert!(
        final_run.judge_verdict.is_none(),
        "rejected verdict must not persist"
    );
    assert!(final_run.winner_lane.is_none());

    common::clear_mock_env();
}

fn sample_report() -> ValidatorOutput {
    ValidatorOutput {
        verdict: Verdict::Pass,
        features: prompts::TextOrList::Text("game loop".to_owned()),
        bugs: prompts::TextOrList::List(vec![]),
        security: prompts::TextOrList::Text("no issues".to_owned()),
        quality_score: 88.5,
        summary: "Detailed validation: implementation matches claims.".to_owned(),
    }
}
