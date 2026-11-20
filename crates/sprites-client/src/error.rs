use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum SpriteError {
    #[error("sprite CLI `{command}` exited with status {code}: {stderr}")]
    CliExit {
        command: String,
        code: i32,
        stderr: String,
        /// Failure round: the captured stdout travels with the error so a
        /// 0-byte harness capture stays visible in the lane evidence (it used
        /// to be dropped with the failed attempt).
        stdout: String,
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
    /// R3-A #1: `sprite info <name>` ran but its output carried no parseable
    /// `URL:` line, so the real (opaque-suffixed) public URL is unknown.
    #[error("sprite info for `{command}` did not report a URL")]
    InfoUrlMissing { command: String },
}
