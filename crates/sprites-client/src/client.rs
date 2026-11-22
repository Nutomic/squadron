use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::{SpriteConfig, SpriteError};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOptions {
    pub env: Vec<(String, String)>,
    pub files: Vec<(PathBuf, PathBuf)>,
    pub dir: Option<PathBuf>,
    pub timeout: Option<Duration>,
}

impl ExecOptions {
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn with_file(mut self, src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Self {
        self.files
            .push((src.as_ref().to_path_buf(), dst.as_ref().to_path_buf()));
        self
    }

    pub fn with_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.dir = Some(dir.as_ref().to_path_buf());
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Wave E: production lane sprites are labeled `squadron=prod` at create —
/// the real CLI takes repeatable `--label key=value` flags, which the platform
/// maps onto the `labels` object of `POST /v1/sprites`.
pub const PROD_SPRITE_LABEL: &str = "squadron=prod";

#[derive(Debug, Clone)]
pub struct SpriteClient {
    config: SpriteConfig,
}

impl SpriteClient {
    pub fn new(config: SpriteConfig) -> Self {
        Self { config }
    }

    pub fn from_env() -> Self {
        Self::new(SpriteConfig::from_env())
    }

    pub fn config(&self) -> &SpriteConfig {
        &self.config
    }

    pub fn name(&self, suffix: &str) -> String {
        self.config.sprite_name(suffix)
    }

    pub async fn create(&self, name: &str) -> Result<CommandOutput, SpriteError> {
        self.run(
            vec![
                "create".into(),
                name.into(),
                "--skip-console".into(),
                "--label".into(),
                PROD_SPRITE_LABEL.into(),
            ],
            None,
        )
        .await
    }

    pub async fn exec(
        &self,
        name: &str,
        cmd: &str,
        options: &ExecOptions,
    ) -> Result<CommandOutput, SpriteError> {
        let mut args = vec!["exec".into(), "-s".into(), name.into(), "--no-stdin".into()];
        for (key, value) in &options.env {
            args.push("--env".into());
            args.push(format!("{key}={value}"));
        }
        for (src, dst) in &options.files {
            args.push("--file".into());
            args.push(format!("{}:{}", src.display(), dst.display()));
        }
        if let Some(dir) = &options.dir {
            args.push("--dir".into());
            args.push(dir.display().to_string());
        }
        args.push("--".into());
        args.push("sh".into());
        args.push("-c".into());
        args.push(cmd.into());
        self.run(args, options.timeout).await
    }

    pub async fn destroy(&self, name: &str) -> Result<CommandOutput, SpriteError> {
        self.run(vec!["destroy".into(), name.into(), "--force".into()], None)
            .await
    }

    pub async fn checkpoint(&self, name: &str) -> Result<CommandOutput, SpriteError> {
        self.run(
            vec!["checkpoint".into(), "create".into(), name.into()],
            None,
        )
        .await
    }

    /// R3-A #2: the auth flag must be scoped to the sprite: real CLI semantics
    /// require `-s <name>`, and a missing name is a hard failure on the
    /// Machine. Callers must assert success instead of discarding the error.
    pub async fn set_url_auth_public(&self, name: &str) -> Result<CommandOutput, SpriteError> {
        self.run(
            vec![
                "config".into(),
                "update".into(),
                "-s".into(),
                name.into(),
                "--url-auth".into(),
                "public".into(),
            ],
            None,
        )
        .await
    }

    /// R3-A #1: the real public URL is NOT `{scheme}://{name}.sprites.app` —
    /// sprite hosts carry an opaque suffix assigned by the platform (e.g.
    /// `swarm-x` -> `swarm-x-b5t5x.sprites.app`). Parse the `URL:` line from
    /// `sprite info <name>` instead of ever fabricating a host.
    pub async fn url(&self, name: &str) -> Result<String, SpriteError> {
        let args = vec!["info".into(), name.into()];
        let output = self.run(args.clone(), None).await?;
        let command = args.join(" ");
        parse_info_url(&output.stdout).ok_or(SpriteError::InfoUrlMissing { command })
    }

    pub async fn list(&self) -> Result<Vec<String>, SpriteError> {
        let output = self.run(vec!["list".into()], None).await?;
        Ok(parse_sprite_names(&output.stdout))
    }

    /// F7: read the authoritative public-routing state from
    /// `sprite info <name>` — the value of its `Auth:` field (e.g.
    /// "public"/"private"). A config update whose output looks successful is
    /// still not proof; callers must verify here before proceeding.
    pub async fn auth(&self, name: &str) -> Result<String, SpriteError> {
        let args = vec!["info".into(), name.into()];
        let output = self.run(args.clone(), None).await?;
        let command = args.join(" ");
        parse_info_auth(&output.stdout).ok_or(SpriteError::InfoAuthMissing { command })
    }

    /// Query the public URL of a sprite (`sprite info <name>`). Returns the
    /// first http(s) URL found in the CLI output, or an empty string when the
    /// CLI prints none (callers fall back to the deterministic name form).
    pub async fn info(&self, name: &str) -> Result<String, SpriteError> {
        let output = self.run(vec!["info".into(), name.into()], None).await?;
        Ok(parse_url(&output.stdout))
    }

    async fn run(
        &self,
        args: Vec<String>,
        timeout_override: Option<Duration>,
    ) -> Result<CommandOutput, SpriteError> {
        let description = describe(&args);
        let timeout = timeout_override.unwrap_or(self.config.command_timeout);
        let mut command = Command::new(&self.config.cli_path);
        command
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().map_err(|source| SpriteError::Spawn {
            command: description.clone(),
            path: self.config.cli_path.display().to_string(),
            source,
        })?;
        let result = tokio::time::timeout(timeout, child.wait_with_output()).await;
        match result {
            Ok(Ok(output)) => {
                if !output.status.success() {
                    return Err(SpriteError::CliExit {
                        command: description,
                        code: output.status.code().unwrap_or(-1),
                        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    });
                }
                Ok(CommandOutput {
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    exit_code: output.status.code().unwrap_or(0),
                })
            }
            Ok(Err(source)) => Err(SpriteError::Spawn {
                command: description,
                path: self.config.cli_path.display().to_string(),
                source,
            }),
            Err(_) => Err(SpriteError::Timeout {
                command: description,
                duration: timeout,
            }),
        }
    }
}

fn describe(args: &[String]) -> String {
    let mut parts = Vec::new();
    let mut sanitize_next = false;
    for arg in args {
        if sanitize_next {
            let key = arg.split('=').next().unwrap_or("?");
            parts.push(format!("{key}=***"));
            sanitize_next = false;
        } else if arg == "--env" {
            parts.push(arg.clone());
            sanitize_next = true;
        } else {
            parts.push(arg.clone());
        }
    }
    parts.join(" ")
}

/// R3-A #1: extract the public URL from `sprite info` output. The URL line is
/// `URL: <https url>` (case-insensitive key, optional table padding); anything
/// else is not a URL we may invent.
pub fn parse_info_url(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        // Table row (`| url | https://... |`): split cells on the pipe so a
        // ':' inside the URL itself cannot break key detection.
        if trimmed.starts_with('|') || trimmed.starts_with('│') {
            let cells: Vec<String> = trimmed
                .trim_matches(|c| c == '|' || c == '│')
                .split(['|', '│'])
                .map(|c| c.trim().to_owned())
                .collect();
            for pair in cells.windows(2) {
                if pair[0].eq_ignore_ascii_case("url") {
                    let url = pair[1].as_str();
                    if url.starts_with("http://") || url.starts_with("https://") {
                        return Some(url.to_owned());
                    }
                }
            }
            continue;
        }
        // Plain `key: value` line.
        let (key, value) = match trimmed.split_once(':') {
            Some(kv) => kv,
            None => continue,
        };
        if !key.trim().eq_ignore_ascii_case("url") {
            continue;
        }
        let url = value.trim();
        if url.starts_with("http://") || url.starts_with("https://") {
            return Some(url.to_owned());
        }
    }
    None
}

/// F7: extract the `Auth:` field from `sprite info` output (the authoritative
/// public-routing state; evidence format: `Auth: public`). Same tolerant
/// shapes as `parse_info_url`: case-insensitive key, optional table padding,
/// non-empty value required.
pub fn parse_info_auth(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('|') || trimmed.starts_with('│') {
            let cells: Vec<String> = trimmed
                .trim_matches(|c| c == '|' || c == '│')
                .split(['|', '│'])
                .map(|c| c.trim().to_owned())
                .collect();
            for pair in cells.windows(2) {
                if pair[0].eq_ignore_ascii_case("auth") && !pair[1].is_empty() {
                    return Some(pair[1].to_owned());
                }
            }
            continue;
        }
        let (key, value) = match trimmed.split_once(':') {
            Some(kv) => kv,
            None => continue,
        };
        if !key.trim().eq_ignore_ascii_case("auth") {
            continue;
        }
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_owned());
        }
    }
    None
}

fn parse_sprite_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in text.lines() {
        let line = line.trim().trim_matches(|c| c == '|' || c == '│');
        if line.is_empty()
            || line.starts_with("NAME")
            || line.starts_with("name")
            || line.chars().all(|c| c == '-' || c == '=' || c == ' ')
        {
            continue;
        }
        if let Some(token) = line.split_whitespace().next() {
            let token = token.trim_matches(|c| c == '|' || c == '│');
            if token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
                && !names.iter().any(|name| name == token)
            {
                names.push(token.to_owned());
            }
        }
    }
    names
}

/// Extract the first http(s) URL from CLI output (used by `sprite info`).
fn parse_url(text: &str) -> String {
    for token in text.split_whitespace() {
        let token =
            token.trim_matches(|c: char| c == ',' || c == '"' || c == '\'' || c == ')' || c == '>');
        if token.starts_with("http://") || token.starts_with("https://") {
            return token.to_owned();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::parse_url;

    #[test]
    fn parse_url_takes_the_first_http_token() {
        assert_eq!(
            parse_url("sprite foo  running  https://foo.sprites.app\n"),
            "https://foo.sprites.app"
        );
        assert_eq!(parse_url("no url here"), "");
        assert_eq!(parse_url(""), "");
    }
}
