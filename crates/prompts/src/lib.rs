pub mod fallback;
pub mod preamble;
pub mod roles;
pub mod schema;

pub use fallback::template_variants;
pub use preamble::{data_block, escape_data_content, preamble, DATA_RULE, PREAMBLE_TEMPLATE};
pub use roles::{diversity_prompt, implementer_prompt, judge_prompt, validator_prompt, Prompt};
pub use schema::{
    gate, gate_and_scan_injections, gate_with_options, injection_markers, parse, sentence_count,
    DiversityOutput, GateOptions, ImplementerOutput, JudgeOutput, LaneJudgeReport, Role,
    SchemaError, TextOrList, ValidatorOutput, Variant, Verdict,
};
