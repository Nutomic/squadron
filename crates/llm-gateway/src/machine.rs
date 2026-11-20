use std::time::Duration;

use reqwest::StatusCode;

use crate::types::DEFAULT_REASONING_EFFORT;
use crate::{ChatRequest, ChatResponse, LlmError, USER_AGENT};

/// Env knob for the thinking effort; defaults to low.
pub const REASONING_EFFORT_ENV: &str = "OPENCODE_REASONING_EFFORT";

pub const MACHINE_ENDPOINT: &str = "https://opencode.ai/inference/go/openai/v1/chat/completions";

#[derive(Debug, Clone)]
pub struct MachineLlmClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    timeout: Duration,
    reasoning_effort: String,
}

impl MachineLlmClient {
    pub fn new(api_key: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: endpoint.into(),
            api_key: api_key.into(),
            timeout: Duration::from_secs(120),
            reasoning_effort: DEFAULT_REASONING_EFFORT.to_owned(),
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

    pub async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        // Enforce the client-level thinking effort on every outbound request.
        let mut request = request.clone();
        request.reasoning_effort = Some(self.reasoning_effort.clone());
        let headers = vec![
            ("user-agent".to_owned(), USER_AGENT.to_owned()),
            (
                "authorization".to_owned(),
                format!("Bearer {}", self.api_key),
            ),
        ];
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
