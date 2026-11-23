//! F9: the curl-floor VALIDATOR turn. The floor validator has no shell and no
//! file access, so its evidence must be pre-embedded in the prompt and it
//! must not echo the implementer's keys (run 7: `unknown field 'done'`,
//! 4x schema-gate failures, junk 0/10 verdicts). This file drives the real
//! curl-floor path with a fake curl that captures the request payload and
//! asserts the evidence + exact-keys/no-echo language reach the model.
#![allow(clippy::unwrap_used, clippy::panic)] // test code: panics on failure are the assertion mechanism

use std::path::PathBuf;
use std::time::Duration;

use llm_gateway::SpriteRecipe;
use orchestrator::harness::{self, HarnessCache, HarnessChoice};
use prompts::data_block;
use sprites_client::{SpriteClient, SpriteConfig};

// Tests here mutate process-global env (PATH, HOME, MOCK_*): serialize.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const VALIDATOR_RESPONSE: &str = r#"{"choices":[{"message":{"content":"{\"verdict\":\"pass\",\"features\":\"site serves from evidence\",\"bugs\":\"none found\",\"security\":\"no issues\",\"quality_score\":90.0,\"summary\":\"evidence-based report\"}"}}],"usage":{"opencode":{"allowance":0.02}}}"#;

fn write_fake(bin: &std::path::Path, name: &str, script: &str) {
    let path = bin.join(name);
    std::fs::write(path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn bin_dir(dir: &std::path::Path) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("/bin/sh", bin.join("sh")).unwrap();
    std::os::unix::fs::symlink("/bin/mkdir", bin.join("mkdir")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/base64", bin.join("base64")).unwrap();
    bin
}

/// Fake curl: passes the probe, fails the opencode install, and for real
/// requests appends the FULL command line (the request payload) to a log file
/// before answering with the canned validator JSON.
fn fake_curl_script(args_log: &std::path::Path) -> String {
    format!(
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo 'curl 9.9 (fake)'
  exit 0
fi
if [ "$1" = "-fsSL" ]; then
  exit 1
fi
printf '%s\n' "$*" >> '{}'
/bin/cat "$FAKE_CURL_RESPONSE"
"#,
        args_log.display()
    )
}

fn exec_count(state: &sprites_client::mock_cli::MockState, name: &str) -> u32 {
    state.sprites.get(name).map(|s| s.exec_count).unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn floor_validator_turn_carries_evidence_and_exact_keys_in_the_request_payload() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = bin_dir(dir.path());
    let args_log = dir.path().join("curl-args.log");
    write_fake(&bin, "curl", &fake_curl_script(&args_log));
    std::env::set_var("PATH", bin.display().to_string());
    std::env::set_var("HOME", dir.path());
    std::env::set_var("MOCK_SPRITE_STATE", dir.path().join("mock-state.json"));
    std::env::set_var("MOCK_COLD_START_MS", "0");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let sprite = SpriteClient::new(SpriteConfig {
        cli_path: option_env!("CARGO_BIN_EXE_mock-sprite-cli")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mock-sprite-cli")
            }),
        command_timeout: Duration::from_secs(10),
        ..SpriteConfig::default()
    });
    sprite.create("val-floor-sprite").await.unwrap();
    sprite
        .set_url_auth_public("val-floor-sprite")
        .await
        .unwrap();

    let response_path = dir.path().join("response.json");
    std::fs::write(&response_path, VALIDATOR_RESPONSE).unwrap();

    // The orchestrator-gathered evidence, embedded as DATA exactly like the
    // pipeline does (bounded file excerpt + HTTP probe).
    let evidence = format!(
        "{}{}",
        data_block(
            "file-index.html",
            "<h1>EVIDENCE-MARKER-pipeline-evidence</h1>\n"
        ),
        data_block(
            "http-probe",
            "GET https://lane.example/ -> HTTP 200\n\n<h1>tunnel</h1>"
        )
    );
    let prompt = prompts::validator_prompt(
        "implementer claims it built the site",
        "https://lane.example/",
        "squadron-work/x/lane-0",
        &evidence,
    );

    let cache = HarnessCache::new();
    let recipe = SpriteRecipe::for_model("floor-val-test", "test-model");
    let run = harness::run_prompt(
        &cache,
        &sprite,
        "val-floor-sprite",
        harness::HarnessPrompt {
            system: &prompt.system,
            user: &prompt.user,
            recipe: &recipe,
            timeout: Duration::from_secs(30),
            connector_env: Some((
                "FAKE_CURL_RESPONSE".to_owned(),
                response_path.to_string_lossy().into_owned(),
            )),
            workdir: Some(dir.path().join("lane-0").to_str().unwrap()),
            role: prompts::Role::Validator,
            retry_backoff: Duration::ZERO,
        },
    )
    .await
    .unwrap();

    assert_eq!(run.choice, HarnessChoice::Curl);
    // The single-turn validator answer passes the VALIDATOR schema gate as-is
    // (no transport stripping needed when the model behaves).
    let parsed: prompts::ValidatorOutput =
        prompts::parse(prompts::Role::Validator, &run.output).unwrap();
    assert_eq!(parsed.verdict, prompts::Verdict::Pass);
    assert_eq!(parsed.quality_score, 90.0);

    // The floor turn's request payload carried the evidence, the exact-keys
    // contract, and the no-echo rule; it did NOT carry the implementer
    // writes/start-server protocol.
    let payload = std::fs::read_to_string(&args_log).unwrap();
    assert!(
        payload.contains("EVIDENCE-MARKER-pipeline-evidence"),
        "evidence must be embedded in the floor validator turn's request payload"
    );
    assert!(
        payload.contains(r#"<data label=\"http-probe\">"#),
        "HTTP-probe evidence must be embedded: {payload}"
    );
    assert!(
        payload.contains(
            "EXACTLY these keys and no others: verdict, features, bugs, security, quality_score, summary"
        ),
        "exact-keys contract missing from the floor validator turn payload"
    );
    assert!(
        payload.contains("Do not include done, files, run_command, or blockers"),
        "no-echo rule missing from the floor validator turn payload"
    );
    assert!(
        payload.contains("DATA blocks embedded in this prompt are your complete evidence"),
        "pre-embedded-evidence statement missing"
    );
    assert!(
        !payload.contains("the orchestrator materializes files for you"),
        "implementer writes protocol must not reach the validator floor turn"
    );
    assert!(
        !payload.contains("start-server.sh"),
        "implementer start-script protocol must not reach the validator floor turn"
    );

    // Bounded session: 5 chain probes + exactly 1 floor turn (the verdict
    // stopped the loop) — and NO start-script exec (validators write no site).
    let state = sprites_client::mock_cli::read_state();
    assert_eq!(exec_count(&state, "val-floor-sprite"), 6);

    std::env::remove_var("PATH");
    std::env::remove_var("HOME");
    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
    std::env::remove_var("MOCK_CREATE_MS");
    std::env::remove_var("MOCK_EXEC_MS");
}
