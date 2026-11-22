use std::time::Duration;

use reqwest::StatusCode;

use crate::types::DEFAULT_REASONING_EFFORT;
use crate::{ChatRequest, ChatResponse, LlmError, USER_AGENT};

/// Env knob for the thinking effort; defaults to low.
pub const REASONING_EFFORT_ENV: &str = "OPENCODE_REASONING_EFFORT";

/// F12: the opencode-go endpoint requires a stable `x-opencode-session` id
/// (400 `MissingSessionID` otherwise). Machine-side calls (judge, diversity
/// generator) use one id per run so routing and prompt caching stay warm
/// within the run. Run IDs may carry characters outside the header's
/// friendliest set; anything not alphanumeric/-/_ becomes '-', capped at 64.
pub fn machine_session_id(run_id: &str) -> String {
    let cleaned: String = run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    format!("squadron-{cleaned}")
}

pub const MACHINE_ENDPOINT: &str = "https://opencode.ai/inference/go/openai/v1/chat/completions";

#[derive(Debug, Clone)]
pub struct MachineLlmClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    timeout: Duration,
    reasoning_effort: String,
    session: Option<String>,
}

impl MachineLlmClient {
    pub fn new(api_key: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: endpoint.into(),
            api_key: api_key.into(),
            timeout: Duration::from_secs(120),
            reasoning_effort: DEFAULT_REASONING_EFFORT.to_owned(),
            session: None,
        }
    }

    pub fn from_env() -> Result<Self, LlmError> {
        let api_key = std::env::var("OPENCODE_KEY")
            .map_err(|_| LlmError::Config("OPENCODE_KEY is not set".to_owned()))?;
        let endpoint =
            std::env::var("OPENCODE_ENDPOINT").unwrap_or_else(|_| MACHINE_ENDPOINT.to_owned());
        let reasoning_effort =
            std::env::var(REASONING_EFFORT_ENV).unwrap_or_else(|_| DEFAULT_REASONING_EFFORT.into());
        Ok(Self::new(api_key, endpoint).with_reasoning_effort(reasoning_effort))
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Config knob for the thinking effort; defaults to low.
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = effort.into();
        self
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn reasoning_effort(&self) -> &str {
        &self.reasoning_effort
    }

    /// F12: stable session id for every outbound request from this client
    /// (required by the opencode-go endpoint; use `machine_session_id(run_id)`).
    pub fn with_session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(session.into());
        self
    }

    /// Per-call session override; wins over the client-level session.
    pub async fn chat_with_session(
        &self,
        request: &ChatRequest,
        session: &str,
    ) -> Result<ChatResponse, LlmError> {
        self.chat_inner(request, Some(session)).await
    }

    /// The configured client-level session, if any.
    pub fn session_id(&self) -> Option<&str> {
        self.session.as_deref()
    }

    pub async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.chat_inner(request, None).await
    }

    async fn chat_inner(
        &self,
        request: &ChatRequest,
        session_override: Option<&str>,
    ) -> Result<ChatResponse, LlmError> {
        // Enforce the client-level thinking effort on every outbound request.
        let mut request = request.clone();
        request.reasoning_effort = Some(self.reasoning_effort.clone());
        let session = session_override.or(self.session.as_deref());
        let mut headers = vec![
            ("user-agent".to_owned(), USER_AGENT.to_owned()),
            (
                "authorization".to_owned(),
                format!("Bearer {}", self.api_key),
            ),
        ];
        if let Some(session) = session {
            headers.push((
                super::sprites_gateway::SESSION_HEADER.to_owned(),
                session.to_owned(),
            ));
        }
        super::transport::post_chat(&self.http, &self.endpoint, &headers, &request, self.timeout)
            .await
    }
}

pub fn is_transient(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
        || status.is_server_error()
}

pub fn truncate(body: &str) -> String {
    const LIMIT: usize = 512;
    if body.chars().count() <= LIMIT {
        return body.to_owned();
    }
    body.chars().take(LIMIT).collect::<String>() + "..."
}
