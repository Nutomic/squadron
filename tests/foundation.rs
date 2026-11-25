#![allow(clippy::unwrap_used)] // test code: panics on failure are the assertion mechanism

use std::time::Duration;

use axum::extract::Json;
use axum::routing::post;
use axum::Router;
use llm_gateway::{ChatRequest, MachineLlmClient};
use prompts::{
    gate_with_options, parse, DiversityOutput, GateOptions, ImplementerOutput, JudgeOutput, Role,
    ValidatorOutput,
};
use serde_json::{json, Value};
use sprites_client::mock_cli::MockState;
use sprites_client::{ExecOptions, SpriteClient, SpriteConfig};
use store::Store;

fn completion(content: &str, cost: f64) -> Value {
    json!({
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"opencode": {"allowance": cost}}
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

#[tokio::test(flavor = "multi_thread")]
async fn foundation_pipeline_composes_against_mock_sprite_and_stub_llm() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("mock-sprites.json");
    std::env::set_var("MOCK_SPRITE_STATE", &state_path);
    std::env::set_var("MOCK_COLD_START_MS", "20");

    let site_listener = match tokio::net::TcpListener::bind(("127.0.0.1", 8080)).await {
        Ok(listener) => listener,
        Err(_) => {
            std::env::remove_var("MOCK_SPRITE_STATE");
            std::env::remove_var("MOCK_COLD_START_MS");
            return;
        }
    };
    let site_server = tokio::spawn(async move {
        let _ = axum::serve(site_listener, sprites_client::mock_http::router()).await;
    });

    let store = Store::connect_in_memory().await.unwrap();
    store.migrate().await.unwrap();
    let brief = "Build a small arcade game";
    let run = store.create_run(brief, 2, 1_700_000_000_000).await.unwrap();
    store.update_run_status(&run.id, "running").await.unwrap();

    let variants = {
        let raw = json!({"variants": prompts::template_variants(2)}).to_string();
        let options = GateOptions {
            expected_variants: Some(2),
            ..GateOptions::default()
        };
        let value = gate_with_options(Role::Diversity, &raw, &options).unwrap();
        let parsed: DiversityOutput = serde_json::from_value(value).unwrap();
        parsed.variants
    };
    assert_eq!(variants.len(), 2);

    let impl_content = json!({
        "summary": "built an arcade page", "files": 3,
        "run_command": "python3 -m http.server 8080", "done": true, "blockers": []
    })
    .to_string();
    let val_content = json!({
        "verdict": "pass", "features": ["score loop"], "bugs": [],
        "security": "no secrets", "quality_score": 82.0, "summary": "verified the game"
    })
    .to_string();
    let judge_content = json!({
        "ranking": [1, 0], "winner": 1, "verdict_card": "Playful and polished. It wins.",
        "reasons": ["better layout"],
        "brief_fulfillment": {
            "state": "fulfilled",
            "evidence": "lane 1's validator report confirms the score loop the brief asked for"
        }
    })
    .to_string();

    let impl_port = serve(Router::new().route(
        "/chat/completions",
        post(move || async move { Json(completion(&impl_content, 0.11)) }),
    ))
    .await;
    let val_port = serve(Router::new().route(
        "/chat/completions",
        post(move || async move { Json(completion(&val_content, 0.07)) }),
    ))
    .await;
    let judge_port = serve(Router::new().route(
        "/chat/completions",
        post(move || async move { Json(completion(&judge_content, 0.2)) }),
    ))
    .await;

    let client = SpriteClient::new(SpriteConfig {
        cli_path: std::path::PathBuf::from(env!("CARGO_BIN_EXE_mock-sprite-cli")),
        command_timeout: Duration::from_secs(5),
        ..SpriteConfig::default()
    });
    let mut total_cost = 0.0;
    for (index, variant) in variants.iter().enumerate() {
        let idx = index as i64;
        store
            .insert_lane(&run.id, idx, &serde_json::to_string(variant).unwrap())
            .await
            .unwrap();
        let name = client.name(&format!("smoke-{}", index));
        client.create(&name).await.unwrap();
        // R3-A: auth is scoped per sprite and the URL is parsed from
        // `sprite info`, never fabricated from the bare name.
        client.set_url_auth_public(&name).await.unwrap();
        let url = client.url(&name).await.unwrap();
        store
            .update_lane_state(&run.id, idx, "creating")
            .await
            .unwrap();
        store
            .start_impl(
                &run.id,
                idx,
                &name,
                &url,
                "glm-5.3-flash",
                1_700_000_001_000,
                1_700_003_601_000,
            )
            .await
            .unwrap();
        store
            .update_lane_state(&run.id, idx, "running")
            .await
            .unwrap();
        client
            .exec(&name, "echo serving", &ExecOptions::default())
            .await
            .unwrap();
        client.checkpoint(&name).await.unwrap();

        let impl_llm = MachineLlmClient::new(
            "test-key",
            format!("http://127.0.0.1:{impl_port}/chat/completions"),
        );
        let response = impl_llm
            .chat(&ChatRequest::new("glm-5.3-flash").user("implement"))
            .await
            .unwrap();
        let output: ImplementerOutput =
            parse(Role::Implementer, &response.text().unwrap()).unwrap();
        store
            .finish_impl(&run.id, idx, &output.summary, output.files as i64)
            .await
            .unwrap();
        total_cost += response.cost_usd().unwrap_or(0.0);

        let val_llm = MachineLlmClient::new(
            "test-key",
            format!("http://127.0.0.1:{val_port}/chat/completions"),
        );
        let response = val_llm
            .chat(&ChatRequest::new("deepseek-v4.1-flash").user("validate"))
            .await
            .unwrap();
        let report: ValidatorOutput = parse(Role::Validator, &response.text().unwrap()).unwrap();
        store
            .finish_validation(
                &run.id,
                idx,
                match report.verdict {
                    prompts::Verdict::Pass => "pass",
                    prompts::Verdict::Fail => "fail",
                },
                &report.summary,
                report.quality_score,
            )
            .await
            .unwrap();
        store
            .update_lane_state(&run.id, idx, "validated")
            .await
            .unwrap();
        total_cost += response.cost_usd().unwrap_or(0.0);
    }

    let judge_llm = MachineLlmClient::new(
        "test-key",
        format!("http://127.0.0.1:{judge_port}/chat/completions"),
    );
    let response = judge_llm
        .chat(&ChatRequest::new(llm_gateway::JUDGE_MODEL).user("judge"))
        .await
        .unwrap();
    let verdict: JudgeOutput = parse(Role::Judge, &response.text().unwrap()).unwrap();
    total_cost += response.cost_usd().unwrap_or(0.0);
    store
        .finish_run(
            &run.id,
            "judged",
            &verdict.verdict_card,
            verdict.winner as i64,
            Some(total_cost),
            1_700_000_100_000,
        )
        .await
        .unwrap();

    let stored_run = store.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(stored_run.status, "judged");
    assert_eq!(stored_run.winner_lane, Some(1));
    assert_eq!(stored_run.total_cost_usd, Some(total_cost));
    let lanes = store.list_lanes(&run.id).await.unwrap();
    assert_eq!(lanes.len(), 2);
    assert!(lanes.iter().all(|lane| lane.state == "validated"));
    assert!(lanes.iter().all(|lane| lane.impl_url.is_some()));

    let state: MockState =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    assert!(state.url_auth_public);
    assert_eq!(state.sprites.len(), 2);
    assert!(state.sprites.values().all(|sprite| sprite.http_200));
    assert!(state.sprites.values().all(|sprite| sprite.checkpoints == 1));

    site_server.abort();
    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
}
