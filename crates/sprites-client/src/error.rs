use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum SpriteError {
    #[error("sprite CLI `{command}` exited with status {code}: {stderr}")]
    CliExit {
        command: String,
        code: i32,
        stderr: String,
    },
    #[error("sprite CLI `{command}` timed out after {duration:?}")]
    Timeout { command: String, duration: Duration },
    #[error("failed to spawn sprite CLI `{command}` at {path}: {source}")]
    Spawn {
        path: String,
        command: String,
        #[source]
        source: std::io::Error,
    },
}
