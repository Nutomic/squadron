use std::time::Duration;

use crate::types::DEFAULT_REASONING_EFFORT;
use crate::{ChatRequest, LlmError, USER_AGENT};

/// #34: canonical connector id; the endpoint URL is derived from it so the
/// id lives in exactly one place (shared with orchestrator's config default).
pub const GATEWAY_CONNECTOR_ID: &str = "sanCSEiLn9Np-G_Y5q2jKA";
pub const GATEWAY_API_BASE: &str = "https://api.sprites.dev/v1/gateway/custom_api";

pub fn gateway_endpoint() -> String {
    format!("{GATEWAY_API_BASE}/{GATEWAY_CONNECTOR_ID}/chat/completions")
}
pub const SESSION_HEADER: &str = "x-opencode-session";
// Model pinning moved to crate::models (single source of truth: implementer
// lanes pinned to glm-5.3-flash, validator deepseek-v4.1-flash, judge glm-5.3).
pub use crate::models::{
    lane_model, DIVERSITY_MODEL, IMPLEMENTER_MODEL, JUDGE_MODEL, LANE_MODELS, VALIDATOR_MODEL,
};

#[derive(Debug, Clone)]
pub struct SpriteRecipe {
    endpoint: String,
    model: String,
    session_id: String,
    timeout: Duration,
    reasoning_effort: String,
}

impl SpriteRecipe {
    pub fn new(lane_id: impl AsRef<str>) -> Self {
        Self::for_model(lane_id, crate::models::IMPLEMENTER_MODEL)
    }

    pub fn for_model(lane_id: impl AsRef<str>, model: impl Into<String>) -> Self {
        Self {
            endpoint: gateway_endpoint(),
            model: model.into(),
            session_id: session_id(lane_id.as_ref()),
            timeout: Duration::from_secs(180),
            reasoning_effort: DEFAULT_REASONING_EFFORT.to_owned(),
        }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
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

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn headers(&self) -> Vec<(String, String)> {
        vec![
            ("user-agent".to_owned(), USER_AGENT.to_owned()),
            (SESSION_HEADER.to_owned(), self.session_id.clone()),
        ]
    }

    pub fn build(&self, system: &str, user: &str) -> ChatRequest {
        ChatRequest::new(self.model.clone())
            .system(system)
            .user(user)
            .reasoning_effort(self.reasoning_effort.clone())
    }

    pub fn curl_command(&self, request: &ChatRequest) -> Result<String, LlmError> {
        let body = serde_json::to_string(request)
            .map_err(|e| LlmError::Request(format!("cannot serialize request: {e}")))?;
        let headers = self
            .headers()
            .into_iter()
            .map(|(key, value)| format!("-H '{key}: {value}'"))
            .collect::<Vec<_>>()
            .join(" ");
        let escaped = body.replace('\'', "'\\''");
        // F17: the HTTP status is written on the last stdout line so the
        // harness curl-loop can classify responses: 5xx/429/408 are transient
        // (retried with backoff), other 4xx (e.g. a schema 400) are permanent
        // and fail fast. Bodies without the suffix still classify by shape.
        Ok(format!(
            "curl -sS --max-time 600 -w '\\n%{{http_code}}' -X POST '{endpoint}' {headers} -H 'content-type: application/json' --data '{escaped}'",
            endpoint = self.endpoint
        ))
    }
}

fn session_id(lane_id: &str) -> String {
    let cleaned: String = lane_id
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
    format!("swarm-{cleaned}")
}
