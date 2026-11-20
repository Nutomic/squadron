use std::time::Duration;

use llm_gateway::{LlmError, SpriteRecipe};
use serde_json::Value;
use sprites_client::{ExecOptions, SpriteClient};
use tokio::sync::RwLock;

/// Ordered coding-agent harness chain inside implementer sprites
/// (spec: opencode CLI -> Codex CLI -> Claude Code -> raw curl-loop floor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessChoice {
    Opencode,
    Codex,
    Claude,
    Curl,
}

pub const HARNESS_CHAIN: [HarnessChoice; 4] = [
    HarnessChoice::Opencode,
    HarnessChoice::Codex,
    HarnessChoice::Claude,
    HarnessChoice::Curl,
];

impl HarnessChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            HarnessChoice::Opencode => "opencode",
            HarnessChoice::Codex => "codex",
            HarnessChoice::Claude => "claude",
            HarnessChoice::Curl => "curl-loop",
        }
    }

    pub fn parse(s: &str) -> Option<HarnessChoice> {
        HARNESS_CHAIN.iter().copied().find(|c| c.as_str() == s)
    }

    fn probe_cmd(self) -> &'static str {
        match self {
            HarnessChoice::Opencode => "command -v opencode >/dev/null 2>&1 && opencode --version",
            HarnessChoice::Codex => "command -v codex >/dev/null 2>&1 && codex --version",
            HarnessChoice::Claude => "command -v claude >/dev/null 2>&1 && claude --version",
            HarnessChoice::Curl => "command -v curl >/dev/null 2>&1 && curl --version",
        }
    }

    /// CLI invocation for the coding agents; `Curl` uses the gateway recipe.
    fn cli_cmd(self, prompt: &str) -> Option<String> {
        let quoted = shell_quote(prompt);
        match self {
            HarnessChoice::Opencode => Some(format!("opencode run {quoted}")),
            HarnessChoice::Codex => Some(format!("codex exec {quoted}")),
            HarnessChoice::Claude => Some(format!("claude -p {quoted}")),
            HarnessChoice::Curl => None,
        }
    }
}

/// Escape a string for a POSIX single-quoted shell argument.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Extract the message text from a raw chat-completions JSON response body.
/// Handles string content and content-parts arrays.
pub fn chat_response_text(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let content = v
        .get("choices")?
        .get(0)?
        .get("message")?
        .get("content")?
        .clone();
    match content {
        Value::String(s) => Some(s),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    text.push_str(t);
                }
            }
            Some(text)
        }
        _ => None,
    }
}

/// Extract usage cost (usage.opencode.allowance) from a raw chat response.
pub fn chat_response_cost(body: &str) -> Option<f64> {
    let v: Value = serde_json::from_str(body).ok()?;
    let usage = v.get("usage")?;
    let oc = usage.get("opencode")?;
    let cost = match oc {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        Value::Object(o) => ["allowance", "cost", "cost_usd", "total"]
            .iter()
            .find_map(|k| o.get(*k).and_then(|x| x.as_f64())),
        _ => None,
    };
    cost.filter(|c| c.is_finite() && *c >= 0.0)
}

/// Process-lifetime cache of the probed harness chain winner. Probed once at
/// the first lane, then reused; invalidated on run-time failure so the next
/// attempt (after sprite recreation) re-probes the chain.
#[derive(Debug, Default)]
pub struct HarnessCache {
    choice: RwLock<Option<HarnessChoice>>,
}

impl HarnessCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn cached(&self) -> Option<HarnessChoice> {
        *self.choice.read().await
    }

    /// Probe the chain in order inside the sprite and cache the winner. The
    /// probe only runs while the cache is empty. The curl-loop floor is always
    /// "available" as long as curl exists.
    pub async fn get_or_probe(
        &self,
        sprite: &SpriteClient,
        name: &str,
    ) -> Result<HarnessChoice, String> {
        if let Some(c) = *self.choice.read().await {
            return Ok(c);
        }
        let mut winner = None;
        for choice in HARNESS_CHAIN {
            let available = matches!(
                sprite.exec(name, choice.probe_cmd(), &ExecOptions::default()).await,
                Ok(out) if out.exit_code == 0
            );
            if available {
                winner = Some(choice);
                break;
            }
        }
        match winner {
            Some(c) => {
                *self.choice.write().await = Some(c);
                Ok(c)
            }
            None => Err("harness: no member of the chain is available (floor failed)".to_owned()),
        }
    }

    /// Drop the cached choice after a run-time failure so the next attempt
    /// re-probes (spec: retry same harness after recreation, then next in list).
    pub async fn invalidate(&self) {
        *self.choice.write().await = None;
    }
}

#[derive(Debug, Clone)]
pub struct HarnessRun {
    pub choice: HarnessChoice,
    pub output: String,
    pub cost_usd: Option<f64>,
}

/// Run one prompt through the probed harness inside the sprite and return the
/// captured output text. When `workdir` is given, the command runs inside that
/// (sprite-side, relative) directory, created on demand.
pub async fn run_prompt(
    cache: &HarnessCache,
    sprite: &SpriteClient,
    name: &str,
    system: &str,
    user: &str,
    recipe: &SpriteRecipe,
    timeout: Duration,
    connector_env: Option<(String, String)>,
    workdir: Option<&str>,
) -> Result<HarnessRun, String> {
    let choice = cache.get_or_probe(sprite, name).await?;
    let prompt_text = format!("{system}\n\n{user}");
    let opts = ExecOptions {
        timeout: Some(timeout),
        ..ExecOptions::default()
    };
    let opts = match connector_env {
        Some((k, v)) => opts.with_env(k, v),
        None => opts,
    };
    let opts = match workdir {
        Some(dir) => opts.with_env("SWARM_WORKDIR", dir),
        None => opts,
    };
    let cd_prefix = |cmd: &str| match workdir {
        Some(_) => format!("mkdir -p \"$SWARM_WORKDIR\" && cd \"$SWARM_WORKDIR\" && {cmd}"),
        None => cmd.to_owned(),
    };
    match choice {
        HarnessChoice::Curl => {
            let request = recipe.build(system, user);
            let curl = recipe
                .curl_command(&request)
                .map_err(|e: LlmError| format!("harness: curl recipe: {e}"))?;
            let out = sprite
                .exec(name, &cd_prefix(&curl), &opts)
                .await
                .map_err(|e| format!("harness[{}]: {e}", choice.as_str()))?;
            let text = chat_response_text(&out.stdout)
                .ok_or_else(|| "harness[curl-loop]: no message text in response".to_owned())?;
            Ok(HarnessRun {
                choice,
                output: text,
                cost_usd: chat_response_cost(&out.stdout),
            })
        }
        _ => {
            let cmd = choice
                .cli_cmd(&prompt_text)
                .expect("non-curl choices have a cli command");
            let out = sprite
                .exec(name, &cd_prefix(&cmd), &opts)
                .await
                .map_err(|e| format!("harness[{}]: {e}", choice.as_str()))?;
            Ok(HarnessRun {
                choice,
                output: out.stdout,
                cost_usd: None,
            })
        }
    }
}

/// All balanced top-level JSON objects in `text`, in order of appearance.
/// Bounded so hostile output cannot make this quadratic.
pub fn json_candidates(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < text.len() && out.len() < 32 {
        let rest = &text[i..];
        if rest.starts_with('{') {
            if let Some(cand) = balanced_object(rest) {
                out.push(cand.clone());
                i += cand.len();
                continue;
            }
        }
        let skip = rest.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        i += skip;
    }
    out
}

fn balanced_object(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                if depth > 0 {
                    depth -= 1;
                    if depth == 0 {
                        return Some(s[..i + 1].to_owned());
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("hi"), "'hi'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("a'b\"c"), "'a'\\''b\"c'");
    }

    #[test]
    fn chain_order_and_names() {
        assert_eq!(HARNESS_CHAIN[0], HarnessChoice::Opencode);
        assert_eq!(HarnessChoice::parse("curl-loop"), Some(HarnessChoice::Curl));
        assert_eq!(HarnessChoice::parse("nope"), None);
    }

    #[test]
    fn chat_response_text_handles_string_and_parts() {
        let s = r#"{"choices":[{"message":{"content":"hello"}}]}"#;
        assert_eq!(chat_response_text(s), Some("hello".to_owned()));
        let p = r#"{"choices":[{"message":{"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}]}"#;
        assert_eq!(chat_response_text(p), Some("ab".to_owned()));
        assert_eq!(chat_response_text("not json"), None);
    }

    #[test]
    fn chat_response_cost_parses_allowance_shapes() {
        assert_eq!(
            chat_response_cost(r#"{"usage":{"opencode":{"allowance":0.125}}}"#),
            Some(0.125)
        );
        assert_eq!(
            chat_response_cost(r#"{"usage":{"opencode":"0.5"}}"#),
            Some(0.5)
        );
        assert_eq!(chat_response_cost(r#"{"usage":{}}"#), None);
    }

    #[test]
    fn json_candidates_finds_last_objects() {
        let text = "noise {\"a\":1} mid {\"b\":{\"c\":2}} tail";
        let cands = json_candidates(text);
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[1], r#"{"b":{"c":2}}"#);
        assert!(json_candidates("no json here").is_empty());
        // unbalanced object is skipped entirely
        assert!(json_candidates("{oops").is_empty());
    }
}
