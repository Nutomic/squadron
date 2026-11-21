#![allow(clippy::unwrap_used, clippy::panic)] // test code: panics on failure are the assertion mechanism

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// Env manipulation must serialize across concurrent async tests; a tokio mutex
// is legal to hold across awaits, a std mutex guard is not.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use sprites_client::mock_cli::{self, MockState};
use sprites_client::{parse_info_url, ExecOptions, SpriteClient, SpriteConfig, SpriteError};

fn fixture_client() -> (SpriteClient, PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv.log");
    let cli = dir.path().join("sprite");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_sprite-fixture"), &cli).unwrap();
    let config = SpriteConfig {
        cli_path: cli,
        name_prefix: "swarm".into(),
        org: "felix-382".into(),
        command_timeout: Duration::from_secs(5),
    };
    (SpriteClient::new(config), log, dir)
}

#[tokio::test]
async fn create_exec_destroy_checkpoint_and_config_use_verified_syntax() {
    let (client, log, _dir) = fixture_client();
    let name = client.name("abc-impl-0");
    assert_eq!(name, "swarm-abc-impl-0");
    assert_eq!(client.config().org, "felix-382");

    client.create(&name).await.unwrap();
    client.checkpoint(&name).await.unwrap();
    client.set_url_auth_public(&name).await.unwrap();
    let url = client.url(&name).await.unwrap();
    assert_eq!(url, format!("https://{name}-f1x2e.sprites.app"));
    client.destroy(&name).await.unwrap();
    let output = client
        .exec(&name, "echo hi", &ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(output.exit_code, 0);

    let lines = fs::read_to_string(&log).unwrap();
    assert!(
        lines.contains("create swarm-abc-impl-0 --skip-console"),
        "{lines}"
    );
    assert!(
        lines.contains("checkpoint create swarm-abc-impl-0"),
        "{lines}"
    );
    assert!(
        lines.contains("config update -s swarm-abc-impl-0 --url-auth public"),
        "{lines}"
    );
    assert!(lines.contains("info swarm-abc-impl-0"), "{lines}");
    assert!(
        lines.contains("destroy swarm-abc-impl-0 --force"),
        "{lines}"
    );
    assert!(
        lines.contains("exec -s swarm-abc-impl-0 --no-stdin -- sh -c echo hi"),
        "{lines}"
    );
}

#[tokio::test]
async fn exec_passes_env_files_and_dir() {
    let (client, log, dir) = fixture_client();
    let src = dir.path().join("source.txt");
    let dst = dir.path().join("dest.txt");
    fs::write(&src, "payload").unwrap();
    let options = ExecOptions::default()
        .with_env("KEY", "value")
        .with_file(&src, &dst)
        .with_dir(dir.path());
    client.exec("sprite", "echo ok", &options).await.unwrap();
    let lines = fs::read_to_string(&log).unwrap();
    assert!(
        lines.contains("exec -s sprite --no-stdin --env KEY=value"),
        "{lines}"
    );
    let expected_file = format!("--file {}:{} --dir", src.display(), dst.display());
    assert!(lines.contains(&expected_file), "{lines}");
    assert!(lines.contains("--dir"), "{lines}");
}

#[tokio::test]
async fn list_parses_sprite_names() {
    let (client, _log, _dir) = fixture_client();
    let names = client.list().await.unwrap();
    assert_eq!(names, vec!["alpha-sprite", "beta-sprite"]);
}

#[tokio::test]
async fn cli_exit_carries_the_captured_stdout() {
    let (client, _log, _dir) = fixture_client();
    let err = client
        .exec("s", "failstdout", &ExecOptions::default())
        .await
        .unwrap_err();
    match err {
        SpriteError::CliExit {
            code,
            stdout,
            stderr,
            ..
        } => {
            assert_eq!(code, 7);
            // Failure round: stdout is captured even on failure (a 0-byte
            // capture must stay distinguishable from a lost one).
            assert_eq!(stdout, "partial stdout before the crash\n");
            assert!(stderr.contains("synthetic failure"));
        }
        other => panic!("expected CliExit, got {other:?}"),
    }
}

#[tokio::test]
async fn error_variants_are_typed() {
    let (client, _log, _dir) = fixture_client();
    let err = client
        .exec("s", "fail", &ExecOptions::default())
        .await
        .unwrap_err();
    match err {
        SpriteError::CliExit {
            code,
            stderr,
            command,
            stdout,
        } => {
            assert_eq!(code, 7);
            assert!(stderr.contains("synthetic failure"));
            assert!(command.contains("sh -c fail"));
            // Failure round: stdout is captured even on failure (here: empty).
            assert!(stdout.is_empty(), "unexpected stdout: {stdout}");
        }
        other => panic!("expected CliExit, got {other:?}"),
    }

    let dir = tempfile::tempdir().unwrap();
    let config = SpriteConfig {
        cli_path: dir.path().join("does-not-exist"),
        ..SpriteConfig::default()
    };
    let missing = SpriteClient::new(config);
    assert!(matches!(
        missing.create("x").await.unwrap_err(),
        SpriteError::Spawn { .. }
    ));

    let (client, _log, _dir) = fixture_client();
    let options = ExecOptions {
        timeout: Some(Duration::from_millis(100)),
        ..ExecOptions::default()
    };
    let start = Instant::now();
    let err = client.exec("s", "sleep 2", &options).await.unwrap_err();
    assert!(matches!(err, SpriteError::Timeout { .. }), "{err:?}");
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn errors_never_leak_env_values() {
    let (client, _log, _dir) = fixture_client();
    let options = ExecOptions::default().with_env("SECRET", "do-not-print");
    let err = client.exec("s", "fail", &options).await.unwrap_err();
    assert!(!err.to_string().contains("do-not-print"), "{err}");
}

#[tokio::test]
async fn mock_cli_models_cold_start_create_exec_and_destroy() {
    let _guard = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    std::env::set_var("MOCK_SPRITE_STATE", &state);
    std::env::set_var("MOCK_COLD_START_MS", "300");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let args = |v: Vec<&str>| v.into_iter().map(str::to_owned).collect::<Vec<_>>();
    mock_cli::execute(&args(vec!["create", "lane0", "--skip-console"])).unwrap();
    // F6 invariant: public url-auth precedes the first exec.
    mock_cli::execute(&args(vec![
        "config",
        "update",
        "-s",
        "lane0",
        "--url-auth",
        "public",
    ]))
    .unwrap();
    let cold_start = Instant::now();
    mock_cli::execute(&args(vec![
        "exec",
        "-s",
        "lane0",
        "--no-stdin",
        "--",
        "sh",
        "-c",
        "echo warm",
    ]))
    .unwrap();
    let cold_elapsed = cold_start.elapsed();
    assert!(cold_elapsed >= Duration::from_millis(300));

    let warm_start = Instant::now();
    mock_cli::execute(&args(vec![
        "exec",
        "-s",
        "lane0",
        "--no-stdin",
        "--",
        "sh",
        "-c",
        "echo warm2",
    ]))
    .unwrap();
    assert!(warm_start.elapsed() < cold_elapsed);
    mock_cli::execute(&args(vec!["checkpoint", "create", "lane0"])).unwrap();
    let listing = mock_cli::execute(&args(vec!["list"])).unwrap();
    assert!(listing.contains("lane0"));
    mock_cli::execute(&args(vec!["destroy", "lane0", "--force"])).unwrap();

    let state: MockState = serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
    assert!(state.url_auth_public);
    assert!(state.sprites.is_empty());
    std::env::remove_var("MOCK_SPRITE_STATE");
    std::env::remove_var("MOCK_COLD_START_MS");
}

#[tokio::test]
async fn mock_cli_create_rejects_existing_sprite_until_destroyed() {
    let _guard = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    std::env::set_var("MOCK_SPRITE_STATE", &state);

    let args = |v: Vec<&str>| v.into_iter().map(str::to_owned).collect::<Vec<_>>();
    mock_cli::execute(&args(vec!["create", "dup", "--skip-console"])).unwrap();
    // Real CLI semantics: creating an existing sprite fails...
    let err = mock_cli::execute(&args(vec!["create", "dup", "--skip-console"])).unwrap_err();
    assert_eq!(err.0, 3, "duplicate create fails");
    // ...until the sprite is destroyed --force.
    mock_cli::execute(&args(vec!["destroy", "dup", "--force"])).unwrap();
    mock_cli::execute(&args(vec!["create", "dup", "--skip-console"])).unwrap();

    std::env::remove_var("MOCK_SPRITE_STATE");
}

/// Real CLI contract (first real run rejection): `sprite create` refuses
/// names with uppercase characters — run-id short hashes are capability
/// tokens with mixed case, so callers must lowercase before naming.
#[tokio::test]
async fn mock_cli_create_rejects_uppercase_names() {
    let _guard = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    std::env::set_var("MOCK_SPRITE_STATE", &state);

    let args = |v: Vec<&str>| v.into_iter().map(str::to_owned).collect::<Vec<_>>();
    let err = mock_cli::execute(&args(vec![
        "create",
        "swarm-oHplwdUQ-impl-0",
        "--skip-console",
    ]))
    .unwrap_err();
    assert_eq!(err.0, 2, "uppercase name is rejected");
    assert!(err.1.contains("lowercase"), "message: {}", err.1);

    let name = "swarm-ohplwdu-impl-0";
    assert!(mock_cli::valid_sprite_name(name));
    mock_cli::execute(&args(vec!["create", name, "--skip-console"])).unwrap();
    let listing = mock_cli::execute(&args(vec!["list"])).unwrap();
    assert!(listing.contains(name));

    std::env::remove_var("MOCK_SPRITE_STATE");
}

#[tokio::test]
async fn client_drives_mock_cli_binary_end_to_end() {
    let _guard = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("binary-state.json");
    std::env::set_var("MOCK_SPRITE_STATE", &state);
    let client = SpriteClient::new(SpriteConfig {
        cli_path: PathBuf::from(env!("CARGO_BIN_EXE_mock-sprite-cli")),
        command_timeout: Duration::from_secs(5),
        ..SpriteConfig::default()
    });
    let name = client.name("smoke");
    client.create(&name).await.unwrap();
    client.set_url_auth_public(&name).await.unwrap();
    let output = client
        .exec(&name, "echo running", &ExecOptions::default())
        .await
        .unwrap();
    assert!(output.stdout.contains("running"));
    client.checkpoint(&name).await.unwrap();
    assert!(client.list().await.unwrap().contains(&name));
    client.destroy(&name).await.unwrap();
    assert!(client.list().await.unwrap().is_empty());
    std::env::remove_var("MOCK_SPRITE_STATE");
}

#[tokio::test(flavor = "multi_thread")]
async fn mock_cli_records_http_200_from_port_8080() {
    let _guard = ENV_LOCK.lock().await;
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 8080)).await {
        Ok(listener) => listener,
        Err(_) => return,
    };
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, sprites_client::mock_http::router()).await;
    });
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("http-state.json");
    std::env::set_var("MOCK_SPRITE_STATE", &state);
    let args = |v: Vec<&str>| v.into_iter().map(str::to_owned).collect::<Vec<_>>();
    mock_cli::execute(&args(vec!["create", "serving", "--skip-console"])).unwrap();
    mock_cli::execute(&args(vec![
        "config",
        "update",
        "-s",
        "serving",
        "--url-auth",
        "public",
    ]))
    .unwrap();
    mock_cli::execute(&args(vec![
        "exec",
        "-s",
        "serving",
        "--no-stdin",
        "--",
        "sh",
        "-c",
        "true",
    ]))
    .unwrap();
    let state: MockState = serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
    assert!(state.sprites["serving"].http_200);
    std::env::remove_var("MOCK_SPRITE_STATE");
    server.abort();
}

/// R3-A #1: the real sprite URL carries an opaque platform suffix; it must be
/// parsed from the `URL:` line of `sprite info`, never fabricated from the
/// bare name.
#[tokio::test(flavor = "multi_thread")]
async fn url_parses_the_suffixed_host_from_sprite_info() {
    let _guard = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("url-state.json");
    std::env::set_var("MOCK_SPRITE_STATE", &state);
    let client = SpriteClient::new(SpriteConfig {
        cli_path: PathBuf::from(env!("CARGO_BIN_EXE_mock-sprite-cli")),
        command_timeout: Duration::from_secs(5),
        ..SpriteConfig::default()
    });
    let name = client.name("urltest");
    client.create(&name).await.unwrap();

    let url = client.url(&name).await.unwrap();
    assert!(
        url.starts_with("https://") && url.ends_with(".sprites.app"),
        "url must be a real host: {url}"
    );
    // the suffix: bare-name form is forbidden
    let bare = format!("https://{name}.sprites.app");
    assert_ne!(url, bare, "the opaque suffix must not be dropped: {url}");
    assert!(
        url.strip_prefix("https://")
            .map(|host| host
                .strip_prefix(&format!("{name}-"))
                .map(|rest| rest != ".sprites.app")
                .unwrap_or(false))
            .unwrap_or(false),
        "host must be <name>-<suffix>.sprites.app: {url}"
    );

    // the URL in the mock state matches what info reported
    let state: MockState = serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(state.sprites[&name].url, url);

    // the bare-name fabrication is a different host than the real one
    assert!(
        !state.sprites[&name]
            .url
            .ends_with(&format!("{name}.sprites.app")),
        "suffixed host must differ from the bare name"
    );
    std::env::remove_var("MOCK_SPRITE_STATE");
}

/// R3-A #1: the parser accepts the documented shapes and nothing else.
#[test]
fn parse_info_url_accepts_url_lines_only() {
    assert_eq!(
        parse_info_url("Name: x\nURL: https://x-b5t5x.sprites.app\nStatus: running\n"),
        Some("https://x-b5t5x.sprites.app".to_owned())
    );
    // lowercase key and table padding
    assert_eq!(
        parse_info_url("| url | https://x-b5t5x.sprites.app |\n"),
        Some("https://x-b5t5x.sprites.app".to_owned())
    );
    // no URL line -> None (never fabricate)
    assert_eq!(parse_info_url("Name: x\nStatus: running\n"), None);
    assert_eq!(parse_info_url(""), None);
    // non-http values are rejected
    assert_eq!(parse_info_url("URL: not-a-url\n"), None);
}

/// R3-A #2: `config update` without `-s <name>` fails (real CLI: "sprite name
/// required"), and a failing url-auth update surfaces as a typed error.
#[tokio::test(flavor = "multi_thread")]
async fn url_auth_without_sprite_name_fails() {
    let _guard = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("auth-state.json");
    std::env::set_var("MOCK_SPRITE_STATE", &state);
    let client = SpriteClient::new(SpriteConfig {
        cli_path: PathBuf::from(env!("CARGO_BIN_EXE_mock-sprite-cli")),
        command_timeout: Duration::from_secs(5),
        ..SpriteConfig::default()
    });

    // the flag must carry the sprite name
    let err = client
        .set_url_auth_public("auth-flag-sprite")
        .await
        .unwrap_err();
    match err {
        SpriteError::CliExit { command, .. } => {
            assert!(
                command.contains("-s auth-flag-sprite"),
                "-s <name> must be passed: {command}"
            );
        }
        other => panic!("expected CliExit, got {other:?}"),
    }

    // create the sprite, then the scoped flag succeeds
    let name = client.name("auth-ok");
    client.create(&name).await.unwrap();
    client.set_url_auth_public(&name).await.unwrap();
    let state: MockState = serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
    assert!(state.url_auth_public);
    std::env::remove_var("MOCK_SPRITE_STATE");
}

/// F6 invariant: a created sprite that never received
/// `sprite config update -s <name> --url-auth public` must fail loudly on its
/// first exec — this is exactly the failure mode that killed real runs (the
/// orchestrator's public 200-check hit the platform auth wall).
#[tokio::test(flavor = "multi_thread")]
async fn exec_before_public_url_auth_fails_loudly() {
    let _guard = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("invariant-state.json");
    std::env::set_var("MOCK_SPRITE_STATE", &state);
    let client = SpriteClient::new(SpriteConfig {
        cli_path: PathBuf::from(env!("CARGO_BIN_EXE_mock-sprite-cli")),
        command_timeout: Duration::from_secs(5),
        ..SpriteConfig::default()
    });
    let name = client.name("noauth");
    client.create(&name).await.unwrap();

    let err = client
        .exec(&name, "echo hi", &ExecOptions::default())
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("no public url-auth")
            && msg.contains(&format!("config update -s {name}"))
            && msg.contains("--url-auth public"),
        "the invariant error must name the exact missing call: {msg}"
    );

    // The exact verified syntax unlocks exec.
    client.set_url_auth_public(&name).await.unwrap();
    let output = client
        .exec(&name, "echo hi", &ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(output.exit_code, 0);
    std::env::remove_var("MOCK_SPRITE_STATE");
}

/// F6: the invariant is per sprite — configuring one lane sprite must not
/// unlock another.
#[tokio::test(flavor = "multi_thread")]
async fn url_auth_invariant_is_per_sprite() {
    let _guard = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("per-sprite-state.json");
    std::env::set_var("MOCK_SPRITE_STATE", &state);
    let client = SpriteClient::new(SpriteConfig {
        cli_path: PathBuf::from(env!("CARGO_BIN_EXE_mock-sprite-cli")),
        command_timeout: Duration::from_secs(5),
        ..SpriteConfig::default()
    });
    let a = client.name("iso-a");
    let b = client.name("iso-b");
    client.create(&a).await.unwrap();
    client.create(&b).await.unwrap();
    client.set_url_auth_public(&a).await.unwrap();

    client
        .exec(&a, "echo a-ok", &ExecOptions::default())
        .await
        .unwrap();
    let err = client
        .exec(&b, "echo b", &ExecOptions::default())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("no public url-auth"),
        "sprite b must still be locked: {err}"
    );
    std::env::remove_var("MOCK_SPRITE_STATE");
}
