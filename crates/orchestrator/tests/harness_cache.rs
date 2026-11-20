use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use llm_gateway::SpriteRecipe;
use orchestrator::harness::{HarnessCache, HarnessChoice};
use sprites_client::mock_cli;
use sprites_client::{SpriteClient, SpriteConfig};

// Tests here rewrite PATH and mock-state env vars: serialize them.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn test_sprite(dir: &std::path::Path, name: &str) -> SpriteClient {
    // Point HOME at the test dir so the opencode re-probe cannot find a real
    // host install under $HOME/.opencode/bin.
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
    sprite
}

fn exec_count(name: &str) -> u32 {
    let state = mock_cli::read_state();
    state.sprites.get(name).map(|s| s.exec_count).unwrap_or(0)
}

/// A PATH holding only a fake `curl` (plus `sh`): every CLI harness member is
/// missing, so the chain must settle on the curl-loop floor.
fn curl_only_path(bin: &std::path::Path) -> String {
    bin.display().to_string()
}

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
    // `sh` runs the commands; `mkdir` is used by run_prompt's workdir prefix
    // and `base64` by the opencode config/file writers.
    std::os::unix::fs::symlink("/bin/sh", bin.join("sh")).unwrap();
    std::os::unix::fs::symlink("/bin/mkdir", bin.join("mkdir")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/base64", bin.join("base64")).unwrap();
    bin
}

const CURL_RESPONSE: &str = r#"{"choices":[{"message":{"content":"curl-floor output"}}],"usage":{"opencode":{"allowance":0.02}}}"#;

fn fake_curl_script() -> &'static str {
    "#!/bin/sh
if [ \"$1\" = \"--version\" ]; then
  echo 'curl 9.9 (fake)'
  exit 0
fi
if [ \"$1\" = \"-fsSL\" ]; then
  # opencode install attempt: fail fast (no install in this test sprite).
  exit 1
fi
/bin/cat \"$FAKE_CURL_RESPONSE\"
"
}

fn fake_opencode_script() -> &'static str {
    "#!/bin/sh
if [ \"$1\" = \"--version\" ]; then
  echo 'opencode 9.9 (fake)'
  exit 0
fi
echo \"$OPENCODE_FAKE_OUTPUT\"
"
}

/// get_or_probe probes the chain exactly once, then serves the cached choice.
/// Chain (R2-2): opencode (probe -> failed install -> re-probe), claude, curl.
#[tokio::test(flavor = "multi_thread")]
async fn get_or_probe_probes_once_then_serves_from_cache() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = bin_dir(dir.path());
    write_fake(&bin, "curl", fake_curl_script());
    std::env::set_var("PATH", curl_only_path(&bin));
    let sprite = test_sprite(dir.path(), "probe-sprite").await;

    let cache = HarnessCache::new();
    assert_eq!(cache.cached().await, None);

    let first = cache.get_or_probe(&sprite, "probe-sprite").await.unwrap();
    assert_eq!(first, HarnessChoice::Curl, "curl floor must win");
    // opencode: probe + failed install + re-probe; claude probe; curl probe.
    assert_eq!(
        exec_count("probe-sprite"),
        5,
        "probe + install per missing member"
    );
    assert_eq!(cache.cached().await, Some(first));

    // Cached: no further sprite execs.
    let again = cache.get_or_probe(&sprite, "probe-sprite").await.unwrap();
    assert_eq!(again, first);
    assert_eq!(exec_count("probe-sprite"), 5, "cache hit must not re-probe");
}

/// invalidate drops the cached choice so the next attempt re-probes.
#[tokio::test(flavor = "multi_thread")]
async fn invalidate_drops_the_cache_and_reprobes() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = bin_dir(dir.path());
    write_fake(&bin, "curl", fake_curl_script());
    std::env::set_var("PATH", curl_only_path(&bin));
    let sprite = test_sprite(dir.path(), "invalidate-sprite").await;

    let cache = HarnessCache::new();
    let first = cache
        .get_or_probe(&sprite, "invalidate-sprite")
        .await
        .unwrap();
    assert_eq!(exec_count("invalidate-sprite"), 5);

    cache.invalidate().await;
    assert_eq!(cache.cached().await, None, "invalidate clears the choice");

    let second = cache
        .get_or_probe(&sprite, "invalidate-sprite")
        .await
        .unwrap();
    assert_eq!(second, first, "the chain re-probes to the same winner");
    assert_eq!(
        exec_count("invalidate-sprite"),
        10,
        "re-probe after invalidation"
    );
}

/// run_prompt drives the curl-loop floor: recipe -> curl command inside the
/// sprite, response text and gateway cost parsed from the body.
#[tokio::test(flavor = "multi_thread")]
async fn run_prompt_runs_the_curl_floor_and_parses_cost() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = bin_dir(dir.path());
    write_fake(&bin, "curl", fake_curl_script());
    std::env::set_var("PATH", curl_only_path(&bin));
    let sprite = test_sprite(dir.path(), "curl-sprite").await;

    let response_path = dir.path().join("response.json");
    std::fs::write(&response_path, CURL_RESPONSE).unwrap();

    let cache = HarnessCache::new();
    let recipe = SpriteRecipe::for_model("run-prompt-test", "test-model");
    let run = orchestrator::harness::run_prompt(
        &cache,
        &sprite,
        "curl-sprite",
        "system prompt",
        "user prompt",
        &recipe,
        Duration::from_secs(30),
        Some((
            "FAKE_CURL_RESPONSE".to_owned(),
            response_path.to_string_lossy().into_owned(),
        )),
        Some("swarm-work/x/lane-0"),
    )
    .await
    .unwrap();

    assert_eq!(run.choice, HarnessChoice::Curl);
    assert_eq!(run.output, "curl-floor output");
    // The floor is a real turn loop (R2-2): with a fixed non-done answer it
    // runs to the 12-turn budget; each turn costs the gateway-reported 0.02.
    let cost = run.cost_usd.unwrap_or(-1.0);
    assert!((cost - 0.24).abs() < 1e-9, "summed turn cost, got {cost}");
    // Five chain probes plus twelve floor turns: seventeen execs.
    assert_eq!(exec_count("curl-sprite"), 17);
}

/// run_prompt on a CLI harness member execs the CLI and returns its stdout.
#[tokio::test(flavor = "multi_thread")]
async fn run_prompt_cli_choice_returns_agent_stdout() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = bin_dir(dir.path());
    write_fake(&bin, "opencode", fake_opencode_script());
    std::env::set_var("PATH", curl_only_path(&bin));
    let sprite = test_sprite(dir.path(), "cli-sprite").await;

    let cache = HarnessCache::new();
    let recipe = SpriteRecipe::for_model("run-prompt-cli-test", "test-model");
    let run = orchestrator::harness::run_prompt(
        &cache,
        &sprite,
        "cli-sprite",
        "system prompt",
        "user prompt",
        &recipe,
        Duration::from_secs(30),
        Some((
            "OPENCODE_FAKE_OUTPUT".to_owned(),
            r#"{"summary":"agent done","files":2,"run_command":"x","done":true,"blockers":[]}"#
                .to_owned(),
        )),
        Some("swarm-work/x/lane-1"),
    )
    .await
    .unwrap();

    assert_eq!(run.choice, HarnessChoice::Opencode);
    assert_eq!(
        run.output.trim(),
        r#"{"summary":"agent done","files":2,"run_command":"x","done":true,"blockers":[]}"#
    );
    assert_eq!(run.cost_usd, None, "CLI harnesses report no gateway cost");
    // Probe + in-lane opencode.json config write + the agent run.
    assert_eq!(exec_count("cli-sprite"), 3);
}

/// No member of the chain available: get_or_probe fails honestly.
#[tokio::test(flavor = "multi_thread")]
async fn get_or_probe_errors_when_no_harness_member_exists() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    // Only `sh` is reachable: no probe can succeed, including the curl floor.
    std::env::set_var("PATH", bin_dir(dir.path()).display().to_string());
    let sprite = test_sprite(dir.path(), "empty-sprite").await;

    let cache = Arc::new(HarnessCache::new());
    let err = cache
        .get_or_probe(&sprite, "empty-sprite")
        .await
        .unwrap_err();
    assert!(
        err.contains("no member of the chain"),
        "error must name the exhausted chain, got: {err}"
    );
    assert_eq!(cache.cached().await, None);
    // opencode probe + failed install + re-probe, claude, curl, curl fallback.
    assert_eq!(exec_count("empty-sprite"), 6, "every member was probed");
}
