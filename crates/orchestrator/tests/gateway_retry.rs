//! F17: the curl floor must classify TRANSIENT sprite-gateway failures
//! (empty body, `{"error":"failed to reach Custom API API"}`, HTTP 5xx,
//! SSE timeout shapes) and retry them with bounded backoff instead of
//! failing the lane on the first bad response — production run
//! 1emxfM1mR9OefNnKfVrK6w lane 1 died on exactly that error. A permanent
//! 4xx (e.g. a schema 400) must still fail fast without retry. The same
//! wrapper serves the implementer, validator, and floor-validator loops
//! (they share the curl-loop code).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics on failure are the assertion mechanism

use std::path::PathBuf;
use std::time::Duration;

use llm_gateway::SpriteRecipe;
use orchestrator::harness::{self, HarnessCache, HarnessChoice};
use sprites_client::mock_cli;
use sprites_client::{SpriteClient, SpriteConfig};

// Tests here rewrite PATH and mock-state env vars: serialize them.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const IMPL_OK: &str = r#"{"choices":[{"message":{"content":"{\"summary\":\"ok\",\"run_command\":\"python3 -m http.server 8080\",\"done\":true,\"blockers\":[],\"writes\":[]}"}}],"usage":{"opencode":{"allowance":0.02}}}"#;
const VAL_OK: &str = r#"{"choices":[{"message":{"content":"{\"verdict\":\"pass\",\"features\":\"site serves\",\"bugs\":\"none\",\"security\":\"none\",\"quality_score\":90.0,\"summary\":\"ok\"}"}}],"usage":{"opencode":{"allowance":0.02}}}"#;
const TRANSIENT_502: &str = r#"{"error":"failed to reach Custom API API"}
502"#;
const PERMANENT_400: &str = r#"{"error":"schema validation failed: unknown field \"summary\""}
400"#;

fn write_fake(bin: &std::path::Path, name: &str, script: &str) {
    let path = bin.join(name);
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn bin_dir(dir: &std::path::Path) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("/bin/sh", bin.join("sh")).unwrap();
    std::os::unix::fs::symlink("/bin/mkdir", bin.join("mkdir")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/base64", bin.join("base64")).unwrap();
    // the retry scripts read their attempt counter with `cat`
    std::os::unix::fs::symlink("/usr/bin/cat", bin.join("cat")).unwrap();
    bin
}

/// Fake curl answering `TRANSIENT_COUNT` transient 502 gateway errors before
/// `OK_BODY` (real curl writes the `-w` HTTP status on the last stdout line,
/// which is what the classifier reads). The counter file path is baked in.
fn fake_retry_script(counter: &std::path::Path, transient_count: usize, ok_body: &str) -> String {
    let ok = format!("{ok_body}\n200");
    format!(
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo 'curl 9.9 (fake)'
  exit 0
fi
if [ "$1" = "-fsSL" ]; then
  exit 1
fi
n=$(cat '{counter}' 2>/dev/null || echo 0)
n=$((n+1))
echo "$n" > '{counter}'
if [ "$n" -le {transient_count} ]; then
  printf '%s' '{transient}'
else
  printf '%s' '{ok}'
fi
"#,
        counter = counter.display(),
        transient_count = transient_count,
        transient = TRANSIENT_502,
        ok = ok
    )
}

/// Fake curl that ALWAYS answers with a permanent 400 rejection.
fn fake_permanent_400_script() -> String {
    format!(
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo 'curl 9.9 (fake)'
  exit 0
fi
if [ "$1" = "-fsSL" ]; then
  exit 1
fi
printf '%s' '{body}'
"#,
        body = PERMANENT_400
    )
}

fn exec_count(name: &str) -> u32 {
    let state = mock_cli::read_state();
    state.sprites.get(name).map(|s| s.exec_count).unwrap_or(0)
}

async fn test_sprite(dir: &std::path::Path, name: &str) -> SpriteClient {
    std::env::set_var("HOME", dir);
    std::env::set_var("MOCK_SPRITE_STATE", dir.join("mock-state.json"));
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
    sprite.create(name).await.unwrap();
    sprite.set_url_auth_public(name).await.unwrap();
    sprite
}

/// (a) One transient gateway error, then success: the lane proceeds and the
/// retry is logged with its attempt number.
#[tokio::test(flavor = "multi_thread")]
async fn transient_gateway_error_then_success_proceeds_the_lane() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = bin_dir(dir.path());
    let counter = dir.path().join("attempts");
    write_fake(&bin, "curl", &fake_retry_script(&counter, 1, IMPL_OK));
    std::env::set_var("PATH", bin.display().to_string());
    let sprite = test_sprite(dir.path(), "retry-ok-sprite").await;

    let cache = HarnessCache::new();
    let recipe = SpriteRecipe::for_model("retry-ok-test", "test-model");
    let run = harness::run_prompt(
        &cache,
        &sprite,
        "retry-ok-sprite",
        harness::HarnessPrompt {
            system: "system prompt",
            user: "user prompt",
            recipe: &recipe,
            timeout: Duration::from_secs(30),
            connector_env: None,
            workdir: Some(dir.path().join("lane-0").to_str().unwrap()),
            role: prompts::Role::Implementer,
            retry_backoff: Duration::ZERO,
        },
    )
    .await
    .unwrap();

    assert_eq!(run.choice, HarnessChoice::Curl);
    assert!(
        run.output.contains("\"done\":true"),
        "output: {}",
        run.output
    );
    // The one transient response was retried, with the attempt number logged.
    assert_eq!(run.retry_log.len(), 1, "retry log: {:?}", run.retry_log);
    assert!(
        run.retry_log[0].contains("attempt 1 of 6"),
        "retry log: {:?}",
        run.retry_log
    );
    assert!(
        run.retry_log[0].contains("failed to reach Custom API API"),
        "retry log must carry the raw response: {:?}",
        run.retry_log
    );
    // Five chain probes + one turn (1 transient attempt + the success) + the
    // detached-server start-script exec.
    assert_eq!(exec_count("retry-ok-sprite"), 8);
}

/// (b) Persistent transient gateway errors: the lane fails only after the
/// retries are exhausted, with a clear error naming the last raw response.
#[tokio::test(flavor = "multi_thread")]
async fn persistent_transient_gateway_errors_exhaust_the_retries() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = bin_dir(dir.path());
    let counter = dir.path().join("attempts");
    // More transient responses than the retry budget can absorb.
    write_fake(&bin, "curl", &fake_retry_script(&counter, 99, IMPL_OK));
    std::env::set_var("PATH", bin.display().to_string());
    let sprite = test_sprite(dir.path(), "retry-exhaust-sprite").await;

    let cache = HarnessCache::new();
    let recipe = SpriteRecipe::for_model("retry-exhaust-test", "test-model");
    let err = harness::run_prompt(
        &cache,
        &sprite,
        "retry-exhaust-sprite",
        harness::HarnessPrompt {
            system: "system prompt",
            user: "user prompt",
            recipe: &recipe,
            timeout: Duration::from_secs(30),
            connector_env: None,
            workdir: None,
            role: prompts::Role::Implementer,
            retry_backoff: Duration::ZERO,
        },
    )
    .await
    .unwrap_err();

    assert!(
        err.msg.contains("retries exhausted"),
        "err must say retries were exhausted: {}",
        err.msg
    );
    assert!(
        err.msg.contains("failed to reach Custom API API"),
        "err must name the last raw response: {}",
        err.msg
    );
    // Every retry was logged with its attempt number (5 retries after the
    // first attempt), and the last capture travels with the failure.
    let attempt = err.last.expect("the failed floor attempt is carried");
    assert_eq!(attempt.retry_log.len(), harness::GATEWAY_RETRIES);
    for (n, line) in attempt.retry_log.iter().enumerate() {
        let expected = format!("attempt {} of {}", n + 1, harness::GATEWAY_RETRIES + 1);
        assert!(line.contains(&expected), "retry line {n}: {line}");
    }
    assert!(attempt.output.contains("failed to reach Custom API API"));
    // Five chain probes + one turn (first attempt + five retries).
    assert_eq!(exec_count("retry-exhaust-sprite"), 11);
}

/// (c) A permanent 400 schema rejection fails fast: no retry is burned on a
/// request the gateway will always refuse.
#[tokio::test(flavor = "multi_thread")]
async fn permanent_400_gateway_error_fails_fast_without_retry() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = bin_dir(dir.path());
    write_fake(&bin, "curl", &fake_permanent_400_script());
    std::env::set_var("PATH", bin.display().to_string());
    let sprite = test_sprite(dir.path(), "permanent-400-sprite").await;

    let cache = HarnessCache::new();
    let recipe = SpriteRecipe::for_model("permanent-400-test", "test-model");
    let err = harness::run_prompt(
        &cache,
        &sprite,
        "permanent-400-sprite",
        harness::HarnessPrompt {
            system: "system prompt",
            user: "user prompt",
            recipe: &recipe,
            timeout: Duration::from_secs(30),
            connector_env: None,
            workdir: None,
            role: prompts::Role::Implementer,
            retry_backoff: Duration::ZERO,
        },
    )
    .await
    .unwrap_err();

    assert!(
        err.msg.contains("permanent") && err.msg.contains("400"),
        "err must name the permanent rejection: {}",
        err.msg
    );
    assert!(
        !err.msg.contains("retries exhausted"),
        "a permanent error must not consume the retry budget: {}",
        err.msg
    );
    assert!(
        err.last.unwrap().retry_log.is_empty(),
        "no retry lines for a permanent error"
    );
    // Five chain probes + exactly ONE curl attempt (fail fast).
    assert_eq!(exec_count("permanent-400-sprite"), 6);
}

/// (d) The validator floor shares the same retry wrapper: a transient error
/// then the verdict-bearing answer proceeds the validator loop.
#[tokio::test(flavor = "multi_thread")]
async fn validator_floor_shares_the_transient_retry_wrapper() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = bin_dir(dir.path());
    let counter = dir.path().join("attempts");
    write_fake(&bin, "curl", &fake_retry_script(&counter, 1, VAL_OK));
    std::env::set_var("PATH", bin.display().to_string());
    let sprite = test_sprite(dir.path(), "retry-val-sprite").await;

    let cache = HarnessCache::new();
    let recipe = SpriteRecipe::for_model("retry-val-test", "test-model");
    let run = harness::run_prompt(
        &cache,
        &sprite,
        "retry-val-sprite",
        harness::HarnessPrompt {
            system: "system prompt",
            user: "user prompt",
            recipe: &recipe,
            timeout: Duration::from_secs(30),
            connector_env: None,
            workdir: Some(dir.path().join("lane-0").to_str().unwrap()),
            role: prompts::Role::Validator,
            retry_backoff: Duration::ZERO,
        },
    )
    .await
    .unwrap();

    assert_eq!(run.choice, HarnessChoice::Curl);
    let parsed: prompts::ValidatorOutput =
        prompts::parse(prompts::Role::Validator, &run.output).unwrap();
    assert_eq!(parsed.verdict, prompts::Verdict::Pass);
    assert_eq!(run.retry_log.len(), 1, "retry log: {:?}", run.retry_log);
    // Five chain probes + one turn (1 transient attempt + the success); the
    // validator writes no site, so no start-script exec.
    assert_eq!(exec_count("retry-val-sprite"), 7);
}
