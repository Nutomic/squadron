use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
    /// Review F5: executed subcommand log ("<sub> <sprite-name>"), in issue
    /// order — lets tests prove pipeline ordering (e.g. url-auth public is
    /// issued right after create, before the implementer session).
    #[serde(default)]
    pub command_log: Vec<String>,
    /// Wave D: org-limit (slot-exhaustion) create failures already emitted.
    /// `MOCK_ORG_LIMIT_FAILS` caps how many are produced per state file, so
    /// queue/requeue paths can be exercised end to end.
    #[serde(default)]
    pub org_limit_failures: u32,
    /// F26: sprite-create rate-limit failures already emitted.
    /// `MOCK_RATE_LIMIT_FAILS` caps how many are produced per state file.
    #[serde(default)]
    pub rate_limit_failures: u32,
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
    /// F6: per-sprite public-routing invariant — `config update -s <name>
    /// --url-auth public` must precede the sprite's first exec (a lane sprite
    /// that execs while still private can never pass the public 200-check).
    #[serde(default)]
    pub url_auth_public: bool,
    /// Wave E: labels applied at create (`--label key=value`, repeatable —
    /// the platform API maps them onto the sprite's `labels` object).
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
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
    // Atomic replace (temp file + rename): unlocked readers (`info`, `list`)
    // must never observe a torn file, which would parse as an empty state and
    // report live sprites as "unknown" (R3-A #1 flake).
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
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
        .truncate(false)
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

/// Opaque per-sprite host suffix (FNV-1a over name + creation time), modeling
/// the platform-assigned suffix on real sprite URLs.
fn url_suffix(name: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= now_ms() as u64;
    h = h.wrapping_mul(0x0000_0100_0000_01b3);
    format!("{:05x}", (h & 0xfffff) as u32)
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

// This module emulates the real `sprite` CLI, whose contract is text on
// stdout/stderr; no other output channel is correct for a CLI mock.
#[allow(clippy::print_stdout, clippy::print_stderr)]
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
    record_command(args);
    match args.first().map(String::as_str) {
        Some("create") => create(args),
        Some("exec") => exec(args),
        Some("destroy") => destroy(args),
        Some("checkpoint") => checkpoint(args),
        Some("config") => config_update(args),
        Some("info") => info(args),
        Some("list") => list(),
        _ => Err((2, format!("mock-sprite-cli: unsupported command: {args:?}"))),
    }
}

/// Append "<sub> <sprite-name>" to MockState::command_log (review F5).
fn record_command(args: &[String]) {
    let sub = args.first().cloned().unwrap_or_default();
    let name = args
        .iter()
        .position(|a| a == "-s")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .or_else(|| args.get(1).cloned())
        .unwrap_or_default();
    let _ = with_state(|state| {
        state.command_log.push(format!("{sub} {name}"));
        Ok(())
    });
}

/// URL a created sprite reports: `MOCK_URL_TEMPLATE` (with a `{name}`
/// placeholder) lets tests point sprites at a local server; otherwise the
/// R3-A #1 platform form with an opaque per-sprite suffix.
fn sprite_url(name: &str) -> String {
    std::env::var("MOCK_URL_TEMPLATE")
        .ok()
        .map(|t| t.replace("{name}", name))
        .unwrap_or_else(|| format!("https://{name}-{}.sprites.app", url_suffix(name)))
}

/// Real CLI contract: sprite names must contain only lowercase letters,
/// numbers, and hyphens, and start/end with a letter or number.
pub fn valid_sprite_name(name: &str) -> bool {
    let chars: Vec<char> = name.chars().collect();
    !chars.is_empty()
        && chars
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        && chars.first().is_some_and(char::is_ascii_alphanumeric)
        && chars.last().is_some_and(char::is_ascii_alphanumeric)
}

fn create(args: &[String]) -> Result<String, (i32, String)> {
    // Real CLI (cobra) grammar: `sprite create [flags] [sprite-name]` — flags
    // may appear anywhere; `-s/--sprite <name>` names the sprite explicitly
    // and `-o/--org <org>` is accepted and ignored. `--label <key=value>` is
    // repeatable and maps onto the API's `labels` object; a value without the
    // `key=value` shape is a usage error (exit 2), like the real CLI.
    let mut name: Option<String> = None;
    let mut skip_console = false;
    let mut labels: BTreeMap<String, String> = BTreeMap::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--skip-console" => {
                skip_console = true;
                i += 1;
            }
            "--label" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| (2, "create: --label needs a key=value value".to_owned()))?
                    .clone();
                let (key, value) = value.split_once('=').ok_or_else(|| {
                    (
                        2,
                        format!("create: invalid label {value:?}: expected key=value"),
                    )
                })?;
                if key.is_empty() {
                    return Err((2, "create: invalid label: empty key".to_owned()));
                }
                labels.insert(key.to_owned(), value.to_owned());
                i += 2;
            }
            "-o" | "--org" => {
                i = args
                    .get(i + 1)
                    .map(|_| i + 2)
                    .ok_or_else(|| (2, "create: -o/--org needs a value".to_owned()))?;
            }
            "-s" | "--sprite" => {
                if name.is_some() {
                    return Err((2, "create: sprite name given more than once".to_owned()));
                }
                name = Some(
                    args.get(i + 1)
                        .ok_or_else(|| (2, "create: -s/--sprite needs a name".to_owned()))?
                        .clone(),
                );
                i += 2;
            }
            positional => {
                if name.is_some() {
                    return Err((2, "create: unexpected extra argument".to_owned()));
                }
                name = Some(positional.to_owned());
                i += 1;
            }
        }
    }
    let name = name.ok_or_else(|| (2, "create: missing name".to_owned()))?;
    if !valid_sprite_name(&name) {
        return Err((
            2,
            "create: sprite name must contain only lowercase letters, numbers, and hyphens, and start/end with a letter or number"
                .to_owned(),
        ));
    }
    if !skip_console {
        return Err((2, "create: --skip-console required".to_owned()));
    }
    delay_ms("MOCK_CREATE_MS", 0);
    // Wave D: emulate the platform's org-concurrency-limit create failure
    // ("org-limit" error shape). The counter is persisted manually because
    // `with_state` only writes state when the closure returns Ok.
    let org_limit = std::env::var("MOCK_ORG_LIMIT_FAILS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    if org_limit > 0 {
        let failed = with_state(|state| {
            if state.org_limit_failures >= org_limit {
                return Ok(false);
            }
            state.org_limit_failures += 1;
            write_state(state)?;
            Ok(true)
        })
        .map_err(|e| (3, e))?;
        if failed {
            return Err((
                3,
                "create: org sprite limit of 8 reached (org-limit)".to_owned(),
            ));
        }
    }
    // F26: emulate the platform's sprite-create rate limit (exact production
    // error shape), capped by `MOCK_RATE_LIMIT_FAILS` per state file.
    let rate_limit = std::env::var("MOCK_RATE_LIMIT_FAILS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    if rate_limit > 0 {
        let failed = with_state(|state| {
            if state.rate_limit_failures >= rate_limit {
                return Ok(false);
            }
            state.rate_limit_failures += 1;
            write_state(state)?;
            Ok(true)
        })
        .map_err(|e| (3, e))?;
        if failed {
            return Err((
                3,
                "create: Sprite creation rate limit exceeded. You can create 10 sprites per minute."
                    .to_owned(),
            ));
        }
    }
    with_state(|state| {
        // Model the real CLI: creating an existing sprite fails until it is
        // destroyed (pipeline retry must destroy --force before recreating).
        if state.sprites.contains_key(&name) {
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
                url: sprite_url(&name),
                labels,
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
        // F6 invariant: a created sprite must have had `sprite config update
        // -s <name> --url-auth public` issued BEFORE its first exec. The mock
        // accepts whatever the pipeline used to do — that is exactly how real
        // runs died on the auth wall — so it now fails loudly instead.
        if !sprite.url_auth_public {
            return Err(format!(
                "exec: sprite {name} has no public url-auth; issue `sprite config update -s {name} --url-auth public` before exec"
            ));
        }
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
    // The direct child's exit is the exec's finish line: a real sprite exec
    // returns when the exec'd process exits, and the revive flow's DETACHED
    // command (`... && nohup sh -c 'server' >/dev/null 2>&1 &`) leaves a
    // forked subshell + server alive that would hold the output pipes open
    // forever. So output is drained by reader threads and the exec returns on
    // `try_wait` instead of blocking on pipe EOF (which `child.output()`
    // does, breaking the "exec returns immediately" contract).
    child
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = child.spawn().map_err(|e| (3, e.to_string()))?;
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let (out_buf, out_done) = drain_pipe(out_pipe);
    let (err_buf, err_done) = drain_pipe(err_pipe);
    let status = loop {
        match child.try_wait().map_err(|e| (3, e.to_string()))? {
            Some(status) => break status,
            // A hung foreground command is bounded by the client-side
            // command timeout (SpriteClient kills the CLI), not here.
            None => thread::sleep(Duration::from_millis(5)),
        }
    };
    // The child's own output is flushed by its exit; give the readers a short
    // grace window to drain it. A detached grandchild keeps them blocked
    // longer — that output is intentionally discarded (it goes to /dev/null
    // by the detach redirect anyway).
    for _ in 0..20 {
        if out_done.load(Ordering::SeqCst) && err_done.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let http_200 = probe_http_200();
    let exit_code = status.code().unwrap_or(-1);
    with_state(|state| {
        if let Some(sprite) = state.sprites.get_mut(&name) {
            sprite.exec_count += 1;
            sprite.http_200 |= http_200;
            sprite.last_command = Some(cmd.clone());
        }
        Ok(())
    })
    .map_err(|e| (3, e))?;

    let out_bytes = out_buf.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let mut text = String::from_utf8_lossy(&out_bytes).into_owned();
    let err_bytes = err_buf.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let stderr = String::from_utf8_lossy(&err_bytes);
    if !stderr.is_empty() {
        text.push_str(&stderr);
    }
    if exit_code != 0 {
        return Err((exit_code, text));
    }
    Ok(text)
}

/// Drain one output pipe into a shared buffer on its own thread. The thread
/// finishes when the pipe closes (EOF); for a detached grandchild that can be
/// long after the exec returned — the caller abandons the thread after its
/// grace window and the process exit reclaims it.
fn drain_pipe(pipe: Option<impl Read + Send + 'static>) -> (Arc<Mutex<Vec<u8>>>, Arc<AtomicBool>) {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(AtomicBool::new(false));
    if let Some(mut pipe) = pipe {
        let buf = buf.clone();
        let done = done.clone();
        thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .extend_from_slice(&chunk[..n]),
                }
            }
            done.store(true, Ordering::SeqCst);
        });
    } else {
        done.store(true, Ordering::SeqCst);
    }
    (buf, done)
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
    if args.get(1).map(String::as_str) != Some("update") {
        return Err((2, format!("config: unsupported args {args:?}")));
    }
    // R3-A #2: real CLI semantics require the sprite to be named explicitly;
    // `sprite config update --url-auth public` without `-s <name>` fails on
    // the Machine ("sprite name required"). The mock refuses too.
    let s_at = match args.iter().position(|a| a == "-s") {
        Some(i) => i,
        None => {
            return Err((
                2,
                "config update: sprite name required (-s <name>)".to_owned(),
            ))
        }
    };
    let name = args.get(s_at + 1).ok_or_else(|| {
        (
            2,
            "config update: sprite name required (-s <name>)".to_owned(),
        )
    })?;
    // argv shape: `config update -s <name> --url-auth public`
    if args.get(s_at + 2).map(String::as_str) != Some("--url-auth")
        || args.get(s_at + 3).map(String::as_str) != Some("public")
    {
        return Err((2, format!("config: unsupported args {args:?}")));
    }
    if std::env::var_os("MOCK_CONFIG_FAIL").is_some() {
        return Err((1, "config update: mock url-auth failure".to_owned()));
    }
    with_state(|state| {
        let sprite = state
            .sprites
            .get_mut(name)
            .ok_or_else(|| format!("config update: unknown sprite {name}"))?;
        sprite.url_auth_public = true;
        // Global mirror kept for state consumers that assert the aggregate.
        state.url_auth_public = true;
        Ok(())
    })
    .map_err(|e| (3, e))?;
    Ok("url-auth set to public\n".to_owned())
}

/// `sprite info <name>`: the URL line is the source of truth for the public
/// host (R3-A #1) and the `Auth:` line is the source of truth for the
/// public-routing state (F7: `private` after create, `public` after
/// `config update -s <name> --url-auth public`). The read takes the same
/// exclusive lock as `with_state` so it can never observe a mid-update state.
fn info(args: &[String]) -> Result<String, (i32, String)> {
    let name = args
        .get(1)
        .ok_or_else(|| (2, "info: missing name".to_owned()))?;
    let (sprite,) = with_state(|state| {
        let sprite = state
            .sprites
            .get(name)
            .cloned()
            .ok_or_else(|| format!("info: unknown sprite {name}"))?;
        Ok((sprite,))
    })
    .map_err(|e| (3, e))?;
    Ok(format!(
        "Name: {}\nStatus: running\nURL: {}\nAuth: {}\n",
        sprite.name,
        sprite.url,
        if sprite.url_auth_public {
            "public"
        } else {
            "private"
        }
    ))
}

fn list() -> Result<String, (i32, String)> {
    let state = with_state(|state| Ok(state.clone())).map_err(|e| (3, e))?;
    let mut text = String::new();
    for sprite in state.sprites.values() {
        text.push_str(&format!(
            "{}  running  {}  url-auth={}\n",
            sprite.name,
            sprite.url,
            if sprite.url_auth_public {
                "public"
            } else {
                "private"
            }
        ));
    }
    Ok(text)
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
