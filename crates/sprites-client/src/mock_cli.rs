use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MockState {
    #[serde(default)]
    pub sprites: BTreeMap<String, MockSprite>,
    #[serde(default)]
    pub url_auth_public: bool,
    #[serde(default)]
    pub transient_failures: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MockSprite {
    pub name: String,
    pub created_at_ms: u128,
    pub cold_started: bool,
    pub exec_count: u32,
    pub http_200: bool,
    pub url: String,
    pub checkpoints: u32,
    pub last_command: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct MockExecResult {
    pub exit_code: i32,
    pub http_200: bool,
}

pub fn state_path() -> PathBuf {
    std::env::var_os("MOCK_SPRITE_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("mock-sprites.json"))
}

pub fn read_state() -> MockState {
    let path = state_path();
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => MockState::default(),
    }
}

pub fn write_state(state: &MockState) -> Result<(), String> {
    let path = state_path();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    let text = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| e.to_string())
}

fn with_state<T>(f: impl FnOnce(&mut MockState) -> Result<T, String>) -> Result<T, String> {
    let path = state_path();
    let lock_path = path.with_file_name(format!(
        "{}.lock",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .open(lock_path)
        .map_err(|e| e.to_string())?;
    lock.lock_exclusive().map_err(|e| e.to_string())?;
    let mut state = read_state();
    let result = f(&mut state);
    if result.is_ok() {
        write_state(&state)?;
    }
    let _ = lock.unlock();
    result
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn delay_ms(name: &str, default: u64) {
    let ms = std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default);
    if ms > 0 {
        thread::sleep(Duration::from_millis(ms));
    }
}

pub fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match execute(&args) {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err((code, message)) => {
            eprintln!("{message}");
            code
        }
    }
}

pub fn execute(args: &[String]) -> Result<String, (i32, String)> {
    match args.first().map(String::as_str) {
        Some("create") => create(args),
        Some("exec") => exec(args),
        Some("destroy") => destroy(args),
        Some("checkpoint") => checkpoint(args),
        Some("config") => config_update(args),
        Some("list") => list(),
        Some("info") => info(args),
        _ => Err((2, format!("mock-sprite-cli: unsupported command: {args:?}"))),
    }
}

/// URL a created sprite reports: `MOCK_URL_TEMPLATE` (with a `{name}`
/// placeholder) lets tests point sprites at a local server; otherwise the
/// deterministic platform form.
fn sprite_url(name: &str) -> String {
    std::env::var("MOCK_URL_TEMPLATE")
        .ok()
        .map(|t| t.replace("{name}", name))
        .unwrap_or_else(|| format!("https://{name}.sprites.app"))
}

fn create(args: &[String]) -> Result<String, (i32, String)> {
    let name = args
        .get(1)
        .ok_or_else(|| (2, "create: missing name".to_owned()))?;
    if !args.iter().any(|a| a == "--skip-console") {
        return Err((2, "create: --skip-console required".to_owned()));
    }
    delay_ms("MOCK_CREATE_MS", 0);
    with_state(|state| {
        // Model the real CLI: creating an existing sprite fails until it is
        // destroyed (pipeline retry must destroy --force before recreating).
        if state.sprites.contains_key(name) {
            return Err(format!("create: sprite {name} already exists"));
        }
        state.sprites.insert(
            name.clone(),
            MockSprite {
                name: name.clone(),
                created_at_ms: now_ms(),
                cold_started: false,
                exec_count: 0,
                http_200: false,
                url: sprite_url(name),
                ..MockSprite::default()
            },
        );
        Ok(())
    })
    .map_err(|e| (3, e))?;
    Ok(format!("created {name}\n"))
}

fn exec(args: &[String]) -> Result<String, (i32, String)> {
    let name = args
        .iter()
        .position(|a| a == "-s")
        .and_then(|i| args.get(i + 1))
        .ok_or_else(|| (2, "exec: -s <name> required".to_owned()))?
        .clone();
    if !args.iter().any(|a| a == "--no-stdin") {
        return Err((2, "exec: --no-stdin required".to_owned()));
    }
    let mut env_vars = Vec::new();
    let mut files = Vec::new();
    let mut dir = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--env" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| (2, "exec: --env needs K=V".to_owned()))?;
                let (key, value) = value
                    .split_once('=')
                    .ok_or_else(|| (2, "exec: --env needs K=V".to_owned()))?;
                env_vars.push((key.to_owned(), value.to_owned()));
                i += 2;
            }
            "--file" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| (2, "exec: --file needs src:dst".to_owned()))?;
                let (src, dst) = value
                    .split_once(':')
                    .ok_or_else(|| (2, "exec: --file needs src:dst".to_owned()))?;
                files.push((src.to_owned(), dst.to_owned()));
                i += 2;
            }
            "--dir" => {
                dir = Some(
                    args.get(i + 1)
                        .ok_or_else(|| (2, "exec: --dir needs a path".to_owned()))?
                        .clone(),
                );
                i += 2;
            }
            "--" => break,
            _ => i += 1,
        }
    }
    let separator = args
        .iter()
        .position(|a| a == "--")
        .ok_or_else(|| (2, "exec: missing `--` separator".to_owned()))?;
    let shell = args
        .get(separator + 1)
        .map(String::as_str)
        .ok_or_else(|| (2, "exec: missing shell".to_owned()))?;
    let flag = args
        .get(separator + 2)
        .map(String::as_str)
        .ok_or_else(|| (2, "exec: missing shell flag".to_owned()))?;
    let cmd = args
        .get(separator + 3)
        .ok_or_else(|| (2, "exec: missing command".to_owned()))?
        .clone();
    if shell != "sh" || flag != "-c" {
        return Err((2, "exec: only `sh -c <cmd>` is supported".to_owned()));
    }

    // Test hook: fail exactly one exec per state file so retry paths can be
    // exercised end to end (a transient platform blip, not a per-sprite fault).
    let fail_first = std::env::var_os("MOCK_FAIL_FIRST_EXEC").is_some();
    if fail_first {
        let already_failed = with_state(|state| {
            let f = state.transient_failures > 0;
            state.transient_failures += 1;
            Ok(f)
        })
        .map_err(|e| (3, e))?;
        if !already_failed {
            return Err((1, format!("exec: temporary mock failure on {name}")));
        }
    }

    let cold = with_state(|state| {
        let sprite = state
            .sprites
            .get_mut(&name)
            .ok_or_else(|| format!("exec: unknown sprite {name}"))?;
        let was_cold = !sprite.cold_started;
        sprite.cold_started = true;
        Ok(was_cold)
    })
    .map_err(|e| (3, e))?;
    if cold {
        delay_ms("MOCK_COLD_START_MS", 0);
    }
    delay_ms("MOCK_EXEC_MS", 0);

    for (src, dst) in &files {
        if let Some(parent) = Path::new(dst).parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| (3, e.to_string()))?;
            }
        }
        fs::copy(src, dst).map_err(|e| (3, e.to_string()))?;
    }

    let mut child = Command::new("sh");
    child.arg("-c").arg(&cmd);
    if let Some(dir) = &dir {
        child.current_dir(dir);
    }
    for (key, value) in &env_vars {
        child.env(key, value);
    }
    let output = child.output().map_err(|e| (3, e.to_string()))?;
    let http_200 = probe_http_200();
    let exit_code = output.status.code().unwrap_or(-1);
    with_state(|state| {
        if let Some(sprite) = state.sprites.get_mut(&name) {
            sprite.exec_count += 1;
            sprite.http_200 |= http_200;
            sprite.last_command = Some(cmd.clone());
        }
        Ok(())
    })
    .map_err(|e| (3, e))?;

    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        text.push_str(&stderr);
    }
    if exit_code != 0 {
        return Err((exit_code, text));
    }
    Ok(text)
}

fn destroy(args: &[String]) -> Result<String, (i32, String)> {
    let name = args
        .get(1)
        .ok_or_else(|| (2, "destroy: missing name".to_owned()))?;
    if !args.iter().any(|a| a == "--force") {
        return Err((2, "destroy: --force required".to_owned()));
    }
    with_state(|state| {
        state.sprites.remove(name);
        Ok(())
    })
    .map_err(|e| (3, e))?;
    Ok(format!("destroyed {name}\n"))
}

fn checkpoint(args: &[String]) -> Result<String, (i32, String)> {
    if args.get(1).map(String::as_str) != Some("create") {
        return Err((2, "checkpoint: expected `create`".to_owned()));
    }
    let name = args
        .get(2)
        .ok_or_else(|| (2, "checkpoint create: missing name".to_owned()))?;
    with_state(|state| {
        let sprite = state
            .sprites
            .get_mut(name)
            .ok_or_else(|| format!("checkpoint: unknown sprite {name}"))?;
        sprite.checkpoints += 1;
        Ok(())
    })
    .map_err(|e| (3, e))?;
    Ok(format!("checkpoint created for {name}\n"))
}

fn config_update(args: &[String]) -> Result<String, (i32, String)> {
    if args.get(1).map(String::as_str) != Some("update")
        || args.get(2).map(String::as_str) != Some("--url-auth")
        || args.get(3).map(String::as_str) != Some("public")
    {
        return Err((2, format!("config: unsupported args {args:?}")));
    }
    with_state(|state| {
        state.url_auth_public = true;
        Ok(())
    })
    .map_err(|e| (3, e))?;
    Ok("url-auth set to public\n".to_owned())
}

fn list() -> Result<String, (i32, String)> {
    let state = read_state();
    let mut text = String::new();
    for sprite in state.sprites.values() {
        text.push_str(&format!(
            "{}  running  {}  url-auth={}\n",
            sprite.name,
            sprite.url,
            if state.url_auth_public {
                "public"
            } else {
                "private"
            }
        ));
    }
    Ok(text)
}

/// `info <name>`: print the sprite's public URL (one token, http(s)).
fn info(args: &[String]) -> Result<String, (i32, String)> {
    let name = args
        .get(1)
        .ok_or_else(|| (2, "info: missing name".to_owned()))?;
    let state = read_state();
    let sprite = state
        .sprites
        .get(name)
        .ok_or_else(|| (3, format!("info: unknown sprite {name}")))?;
    Ok(format!("sprite {}  running  {}\n", sprite.name, sprite.url))
}

fn probe_http_200() -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], 8080));
    for _ in 0..5 {
        let mut stream = match TcpStream::connect_timeout(&address, Duration::from_millis(200)) {
            Ok(stream) => stream,
            Err(_) => {
                thread::sleep(Duration::from_millis(25));
                continue;
            }
        };
        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
        if stream
            .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .is_ok()
        {
            let mut buffer = [0u8; 64];
            if let Ok(n) = stream.read(&mut buffer) {
                let response = String::from_utf8_lossy(&buffer[..n]);
                if response.starts_with("HTTP/1.0 200") || response.starts_with("HTTP/1.1 200") {
                    return true;
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}
