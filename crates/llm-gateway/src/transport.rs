use std::time::Duration;

use reqwest::Client;

use crate::{machine, ChatRequest, ChatResponse, LlmError};

pub async fn post_chat(
    http: &Client,
    endpoint: &str,
    headers: &[(String, String)],
    request: &ChatRequest,
    timeout: Duration,
) -> Result<ChatResponse, LlmError> {
    let mut last_error: Option<LlmError> = None;
    for attempt in 0..2 {
        let mut builder = http.post(endpoint).timeout(timeout).json(request);
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        match builder.send().await {
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                if status.is_success() {
                    return serde_json::from_str::<ChatResponse>(&body).map_err(|error| {
                        LlmError::Request(format!("invalid response JSON from {endpoint}: {error}"))
                    });
                }
                let error = LlmError::Http {
                    status: status.as_u16(),
                    body: machine::truncate(&body),
                };
                if attempt == 0 && machine::is_transient(status) {
                    last_error = Some(error);
                } else {
                    return Err(error);
                }
            }
            Err(error) => {
                let error = if error.is_timeout() {
                    LlmError::Timeout(timeout)
                } else {
                    LlmError::Request(format!("request to {endpoint} failed: {error}"))
                };
                if attempt == 0 {
                    last_error = Some(error);
                } else {
                    return Err(error);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    Err(last_error.unwrap_or_else(|| LlmError::Request("retry exhausted".to_owned())))
}
