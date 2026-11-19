use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

static ENV_LOCK: Mutex<()> = Mutex::new(());

use sprites_client::mock_cli::{self, MockState};
use sprites_client::{ExecOptions, SpriteClient, SpriteConfig, SpriteError};

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
    client.set_url_auth_public().await.unwrap();
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
    assert!(lines.contains("config update --url-auth public"), "{lines}");
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
        } => {
            assert_eq!(code, 7);
            assert!(stderr.contains("synthetic failure"));
            assert!(command.contains("sh -c fail"));
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

#[test]
fn mock_cli_models_cold_start_create_exec_and_destroy() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    std::env::set_var("MOCK_SPRITE_STATE", &state);
    std::env::set_var("MOCK_COLD_START_MS", "300");
    std::env::set_var("MOCK_CREATE_MS", "0");
    std::env::set_var("MOCK_EXEC_MS", "0");

    let args = |v: Vec<&str>| v.into_iter().map(str::to_owned).collect::<Vec<_>>();
    mock_cli::execute(&args(vec!["create", "lane0", "--skip-console"])).unwrap();
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
    assert!(cold_start.elapsed() >= Duration::from_millis(300));

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
    assert!(warm_start.elapsed() < Duration::from_millis(200));
    mock_cli::execute(&args(vec!["checkpoint", "create", "lane0"])).unwrap();
    mock_cli::execute(&args(vec!["config", "update", "--url-auth", "public"])).unwrap();
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
async fn client_drives_mock_cli_binary_end_to_end() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    client.set_url_auth_public().await.unwrap();
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
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
