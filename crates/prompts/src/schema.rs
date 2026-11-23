use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Diversity,
    Implementer,
    Validator,
    Judge,
    Decompose,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Diversity => "diversity generator",
            Self::Implementer => "implementer",
            Self::Validator => "validator",
            Self::Judge => "judge",
            Self::Decompose => "brief decomposer",
        }
    }

    pub fn schema(&self) -> Value {
        match self {
            Self::Diversity => json!({
                "type": "object",
                "properties": {
                    "variants": {"type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "angle": {"type": "string"},
                            "stack": {"type": "string"},
                            "tone": {"type": "string"},
                            "constraints": {"type": "array", "items": {"type": "string"}}
                        },
                        "required": ["angle", "stack", "tone", "constraints"],
                        "additionalProperties": false
                    }}
                },
                "required": ["variants"],
                "additionalProperties": false
            }),
            Self::Implementer => json!({
                "type": "object",
                "properties": {
                    "summary": {"type": "string"},
                    "files": {"type": "integer", "minimum": 0},
                    "run_command": {"type": "string"},
                    "done": {"type": "boolean"},
                    "blockers": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["summary", "files", "run_command", "done", "blockers"],
                "additionalProperties": false
            }),
            Self::Validator => json!({
                "type": "object",
                "properties": {
                    "verdict": {"type": "string", "enum": ["pass", "fail"]},
                    "features": {"type": "array", "items": {"type": "string"}},
                    "bugs": {"type": "array", "items": {"type": "string"}},
                    "security": {"type": "array", "items": {"type": "string"}},
                    "quality_score": {"type": "number", "minimum": 0, "maximum": 100},
                    "summary": {"type": "string"}
                },
                "required": ["verdict", "features", "bugs", "security", "quality_score", "summary"],
                "additionalProperties": false
            }),
            Self::Judge => json!({
                "type": "object",
                "properties": {
                    "ranking": {"type": "array", "items": {"type": "integer", "minimum": 0}},
                    "winner": {"type": "integer", "minimum": 0},
                    "verdict_card": {"type": "string", "maxLength": 320},
                    "reasons": {"oneOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}]},
                    "brief_fulfillment": {
                        "type": "object",
                        "properties": {
                            "state": {"type": "string", "enum": ["fulfilled", "unfulfilled", "unverifiable"]},
                            "evidence": {"type": "string"}
                        },
                        "required": ["state", "evidence"],
                        "additionalProperties": false
                    }
                },
                "required": ["ranking", "winner", "verdict_card", "reasons", "brief_fulfillment"],
                "additionalProperties": false
            }),
            // Wave B: brief decomposition turns the user's brief into distinct
            // per-case implementer prompts; the case count decides the lane count.
            Self::Decompose => json!({
                "type": "object",
                "properties": {
                    "cases": {"type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "label": {"type": "string"},
                            "impl_prompt": {"type": "string"}
                        },
                        "required": ["label", "impl_prompt"],
                        "additionalProperties": false
                    }}
                },
                "required": ["cases"],
                "additionalProperties": false
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    pub angle: String,
    pub stack: String,
    pub tone: String,
    pub constraints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiversityOutput {
    pub variants: Vec<Variant>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImplementerOutput {
    pub summary: String,
    pub files: u32,
    pub run_command: String,
    pub done: bool,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Pass,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TextOrList {
    Text(String),
    List(Vec<String>),
}

impl TextOrList {
    pub fn values(&self) -> Vec<String> {
        match self {
            Self::Text(text) => vec![text.clone()],
            Self::List(items) => items.clone(),
        }
    }

    pub fn text(&self) -> String {
        self.values().join("\n")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatorOutput {
    pub verdict: Verdict,
    /// F18: concrete report entries. Pre-F18 reports stored a single prose
    /// string here (see `parse_validator_report` for reading those back).
    pub features: Vec<String>,
    pub bugs: Vec<String>,
    pub security: Vec<String>,
    pub quality_score: f64,
    pub summary: String,
}

/// A validator report paired with the lane index it belongs to, so the judge
/// can crown winners by explicit lane identity instead of report position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneJudgeReport {
    pub lane_idx: usize,
    #[serde(flatten)]
    pub report: ValidatorOutput,
}

impl LaneJudgeReport {
    pub fn new(lane_idx: usize, report: ValidatorOutput) -> Self {
        Self { lane_idx, report }
    }
}

/// Brief-fulfillment states for the judge's verdict (Wave A). Calibration
/// follows the validator rules: `unfulfilled` requires AFFIRMATIVE evidence
/// of a missed requirement; "cannot verify from evidence" is NOT
/// unfulfilled — it is the separate `unverifiable` state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BriefFulfillmentState {
    Fulfilled,
    Unfulfilled,
    Unverifiable,
}

/// The judge's brief-fulfillment verdict for the winning lane. `evidence`
/// must cite something concrete from the winner's site extract or validator
/// report — the gate rejects empty evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BriefFulfillment {
    pub state: BriefFulfillmentState,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeOutput {
    pub ranking: Vec<usize>,
    pub winner: usize,
    pub verdict_card: String,
    pub reasons: TextOrList,
    pub brief_fulfillment: BriefFulfillment,
}

/// Wave B: one decomposition case — a distinct interpretation of the brief
/// whose `impl_prompt` is a FULL self-contained implementer prompt (brief +
/// the case's distinct angle + the standard implementer site constraints).
/// Both fields are untrusted DATA downstream (they feed implementer prompts
/// and are injection-scanned like any agent output).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecomposeCase {
    pub label: String,
    pub impl_prompt: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecomposeOutput {
    pub cases: Vec<DecomposeCase>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GateOptions {
    pub expected_variants: Option<usize>,
    pub max_verdict_sentences: Option<usize>,
    /// Judge only: the exact set of lane indices the judge may rank/crown.
    /// When set, `ranking` must be a permutation of these indices.
    pub expected_lane_indices: Option<Vec<usize>>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SchemaError {
    #[error("agent output is not valid JSON: {0}")]
    InvalidJson(String),
    #[error("agent output does not conform to the {role} schema: {detail}")]
    NonConforming { role: String, detail: String },
}

pub fn gate(role: Role, raw: &str) -> Result<Value, SchemaError> {
    gate_with_options(role, raw, &GateOptions::default())
}

/// The validator report fields that must be arrays of strings.
const VALIDATOR_LIST_FIELDS: [&str; 3] = ["features", "bugs", "security"];

/// Repair a bare-string `features`/`bugs`/`security` into a single-entry
/// array (bounded repair) so lenient model output still conforms to the
/// array schema. Non-string values pass through untouched.
fn repair_validator_report_arrays(mut value: Value) -> Value {
    if let Some(obj) = value.as_object_mut() {
        for field in VALIDATOR_LIST_FIELDS {
            if let Some(entry) = obj.remove(field) {
                let repaired = match entry {
                    Value::String(text) => Value::Array(vec![Value::String(text)]),
                    other => other,
                };
                obj.insert(field.to_owned(), repaired);
            }
        }
    }
    value
}

/// F18 (run hS9b9gsiSJKxeAgBcaYcHQ): validators wrote literal "see array"
/// placeholders instead of report entries. Such text carries no information —
/// the gate rejects it and report readers drop it.
fn is_placeholder_entry(entry: &str) -> bool {
    entry.trim().eq_ignore_ascii_case("see array")
}

/// Hydrate a persisted validator report (`val_report_json`). Tolerates legacy
/// reports whose features/bugs/security were a single prose string (repaired
/// into a single-entry array) and drops empty/placeholder entries that would
/// render as noise.
pub fn parse_validator_report(json: &str) -> Option<ValidatorOutput> {
    let parsed = serde_json::from_str::<ValidatorOutput>(json)
        .ok()
        .or_else(|| {
            let legacy: LegacyValidatorReport = serde_json::from_str(json).ok()?;
            Some(ValidatorOutput {
                verdict: legacy.verdict,
                features: legacy.features.values(),
                bugs: legacy.bugs.values(),
                security: legacy.security.values(),
                quality_score: legacy.quality_score,
                summary: legacy.summary,
            })
        })?;
    let mut parsed = parsed;
    for entries in [&mut parsed.features, &mut parsed.bugs, &mut parsed.security] {
        entries.retain(|entry| !entry.trim().is_empty() && !is_placeholder_entry(entry));
    }
    Some(parsed)
}

/// Pre-F18 row shape: the report arrays were `TextOrList` (string or array).
/// Used only to read legacy `val_report_json` values.
#[derive(Debug, Clone, Deserialize)]
struct LegacyValidatorReport {
    verdict: Verdict,
    features: TextOrList,
    bugs: TextOrList,
    security: TextOrList,
    quality_score: f64,
    summary: String,
}

pub fn gate_with_options(
    role: Role,
    raw: &str,
    options: &GateOptions,
) -> Result<Value, SchemaError> {
    let value: Value =
        serde_json::from_str(raw).map_err(|error| SchemaError::InvalidJson(error.to_string()))?;
    // F18 (run hS9b9gsiSJKxeAgBcaYcHQ): the validator report arrays must be
    // arrays of strings. A bare string (the old lenient shape) is repaired
    // into a single-entry array so an otherwise-good report survives the
    // gate; placeholder text like "see array" is rejected below instead of
    // being persisted and rendered.
    let value = if role == Role::Validator {
        repair_validator_report_arrays(value)
    } else {
        value
    };
    let type_name = role.as_str().to_owned();
    let result = match role {
        Role::Diversity => serde_json::from_value::<DiversityOutput>(value.clone())
            .map_err(|error| format!("{error}"))
            .and_then(|parsed| {
                if parsed.variants.is_empty() {
                    return Err("variants must not be empty".to_owned());
                }
                if let Some(expected) = options.expected_variants {
                    if parsed.variants.len() != expected {
                        return Err(format!(
                            "expected {expected} variants, got {}",
                            parsed.variants.len()
                        ));
                    }
                }
                Ok(())
            }),
        Role::Implementer => serde_json::from_value::<ImplementerOutput>(value.clone())
            .map_err(|error| format!("{error}"))
            .and_then(|parsed| {
                if parsed.summary.trim().is_empty() {
                    return Err("summary must not be empty".to_owned());
                }
                Ok(())
            }),
        Role::Validator => serde_json::from_value::<ValidatorOutput>(value.clone())
            .map_err(|error| format!("{error}"))
            .and_then(|parsed| {
                if !(0.0..=100.0).contains(&parsed.quality_score) {
                    return Err("quality_score must be between 0 and 100".to_owned());
                }
                if parsed.summary.trim().is_empty() {
                    return Err("summary must not be empty".to_owned());
                }
                for (field, entries) in [
                    ("features", &parsed.features),
                    ("bugs", &parsed.bugs),
                    ("security", &parsed.security),
                ] {
                    if let Some(entry) = entries.iter().find(|e| is_placeholder_entry(e)) {
                        return Err(format!(
                            "{field} must list concrete entries (may be empty), never placeholder text like {entry:?}"
                        ));
                    }
                }
                Ok(())
            }),
        Role::Judge => serde_json::from_value::<JudgeOutput>(value.clone())
            .map_err(|error| format!("{error}"))
            .and_then(|parsed| {
                if !parsed.ranking.contains(&parsed.winner) {
                    return Err("winner must appear in ranking".to_owned());
                }
                if let Some(expected) = &options.expected_lane_indices {
                    if !is_permutation(&parsed.ranking, expected) {
                        return Err(format!(
                            "ranking must be a permutation of lane indices {expected:?}"
                        ));
                    }
                }
                let limit = options.max_verdict_sentences.unwrap_or(2);
                if sentence_count(&parsed.verdict_card) > limit {
                    return Err(format!("verdict_card must be at most {limit} sentences"));
                }
                if parsed.brief_fulfillment.evidence.trim().is_empty() {
                    return Err(
                        "brief_fulfillment.evidence must cite concrete evidence from the lane's site or validator report"
                            .to_owned(),
                    );
                }
                Ok(())
            }),
        Role::Decompose => serde_json::from_value::<DecomposeOutput>(value.clone())
            .map_err(|error| format!("{error}"))
            .and_then(|parsed| {
                if parsed.cases.is_empty() {
                    return Err("cases must not be empty".to_owned());
                }
                for case in &parsed.cases {
                    if case.label.trim().is_empty() {
                        return Err("case label must not be empty".to_owned());
                    }
                    if case.impl_prompt.trim().is_empty() {
                        return Err("case impl_prompt must not be empty".to_owned());
                    }
                }
                Ok(())
            }),
    };
    result
        .map(|()| value)
        .map_err(|detail| SchemaError::NonConforming {
            role: type_name,
            detail,
        })
}

pub fn parse<T: DeserializeOwned>(role: Role, raw: &str) -> Result<T, SchemaError> {
    let value = gate(role, raw)?;
    serde_json::from_value(value).map_err(|error| SchemaError::NonConforming {
        role: role.as_str().to_owned(),
        detail: error.to_string(),
    })
}

pub fn sentence_count(text: &str) -> usize {
    text.split(['.', '!', '?'])
        .filter(|piece| !piece.trim().is_empty())
        .count()
}

fn is_permutation(actual: &[usize], expected: &[usize]) -> bool {
    actual.len() == expected.len()
        && expected
            .iter()
            .all(|idx| actual.iter().filter(|a| **a == *idx).count() == 1)
}

pub fn injection_markers(value: &Value) -> Vec<String> {
    let mut found = Vec::new();
    collect_markers(value, "$", &mut found);
    found
}

/// Pipeline integration point (#21): call this instead of a bare `gate` after
/// every role output so `[INJECTION?]` markers reported by agents are surfaced
/// instead of silently dropped. Returns the JSON paths containing markers.
pub fn gate_and_scan_injections(
    role: Role,
    raw: &str,
    options: &GateOptions,
) -> Result<(Value, Vec<String>), SchemaError> {
    let value = gate_with_options(role, raw, options)?;
    let markers = injection_markers(&value);
    Ok((value, markers))
}

fn collect_markers(value: &Value, path: &str, found: &mut Vec<String>) {
    match value {
        Value::String(text) if text.contains("[INJECTION?]") => found.push(path.to_owned()),
        Value::Object(map) => {
            for (key, child) in map {
                collect_markers(child, &format!("{path}.{key}"), found);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                collect_markers(child, &format!("{path}[{index}]"), found);
            }
        }
        _ => {}
    }
}
