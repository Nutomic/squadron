use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const USER_AGENT: &str = "sprite-swarm/0.1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            messages: Vec::new(),
            temperature: None,
            max_tokens: None,
        }
    }

    pub fn system(mut self, content: impl Into<String>) -> Self {
        self.messages.push(ChatMessage::system(content));
        self
    }

    pub fn user(mut self, content: impl Into<String>) -> Self {
        self.messages.push(ChatMessage::user(content));
        self
    }

    pub fn message(mut self, message: ChatMessage) -> Self {
        self.messages.push(message);
        self
    }

    pub fn temperature(mut self, value: f32) -> Self {
        self.temperature = Some(value);
        self
    }

    pub fn max_tokens(mut self, value: u32) -> Self {
        self.max_tokens = Some(value);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: MessageContent,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::new("system", MessageContent::Text(content.into()))
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new("user", MessageContent::Text(content.into()))
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new("assistant", MessageContent::Text(content.into()))
    }

    pub fn user_parts(parts: Vec<ContentPart>) -> Self {
        Self::new("user", MessageContent::Parts(parts))
    }

    pub fn new(role: impl Into<String>, content: MessageContent) -> Self {
        Self {
            role: role.into(),
            content,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl MessageContent {
    pub fn text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Parts(parts) => parts
                .iter()
                .filter_map(|part| part.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<ImageUrl>,
}

impl ContentPart {
    pub fn text(value: impl Into<String>) -> Self {
        Self {
            kind: "text".to_owned(),
            text: Some(value.into()),
            image_url: None,
        }
    }

    pub fn image(url: impl Into<String>) -> Self {
        Self {
            kind: "image_url".to_owned(),
            text: None,
            image_url: Some(ImageUrl {
                url: url.into(),
                detail: Some("high".to_owned()),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub id: Option<String>,
    pub model: Option<String>,
    pub choices: Vec<Choice>,
    pub usage: Option<Usage>,
}

impl ChatResponse {
    pub fn text(&self) -> Option<String> {
        self.choices
            .first()
            .map(|choice| choice.message.content.text())
            .filter(|text| !text.is_empty())
    }

    pub fn cost_usd(&self) -> Option<f64> {
        self.usage.as_ref().and_then(Usage::cost_usd)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub index: i32,
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub opencode: Option<OpenCodeUsage>,
}

impl Usage {
    pub fn cost_usd(&self) -> Option<f64> {
        self.opencode
            .as_ref()
            .and_then(|open| parse_allowance(open.allowance.as_ref()?))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenCodeUsage {
    pub allowance: Option<Value>,
}

pub fn parse_allowance(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        Value::Object(object) => {
            for key in ["usd", "cost_usd", "cost", "amount", "total", "value"] {
                if let Some(found) = object.get(key).and_then(parse_allowance) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}
