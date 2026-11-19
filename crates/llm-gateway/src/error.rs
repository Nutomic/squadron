#[derive(Debug, Clone, thiserror::Error)]
pub enum LlmError {
    #[error("LLM request failed: {0}")]
    Request(String),
    #[error("LLM returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("LLM response contained no assistant message")]
    EmptyResponse,
    #[error("LLM configuration error: {0}")]
    Config(String),
    #[error("LLM request timed out after {0:?}")]
    Timeout(std::time::Duration),
}
