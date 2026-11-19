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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

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
            vec!["create".into(), name.into(), "--skip-console".into()],
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

    pub async fn set_url_auth_public(&self) -> Result<CommandOutput, SpriteError> {
        self.run(
            vec![
                "config".into(),
                "update".into(),
                "--url-auth".into(),
                "public".into(),
            ],
            None,
        )
        .await
    }

    pub async fn list(&self) -> Result<Vec<String>, SpriteError> {
        let output = self.run(vec!["list".into()], None).await?;
        Ok(parse_sprite_names(&output.stdout))
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
