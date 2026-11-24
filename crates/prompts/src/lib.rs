pub mod fallback;
pub mod preamble;
pub mod roles;
pub mod schema;

pub use fallback::{template_case_prompt, template_variants};
pub use preamble::{
    data_block, data_rule, neutralize_data_closers, preamble, DATA_RULE, PREAMBLE_TEMPLATE,
    PROSE_DATA_RULE, PROSE_REPORT_RULE,
};
pub use roles::{
    decompose_prompt, diversity_prompt, impl_prompt_with_constraints, implementer_case_prompt,
    implementer_prompt, judge_prompt, planning_first_rule, validator_prompt, Prompt,
    CASE_CONSTRAINTS_LINE, IMPLEMENTER_SITE_CONSTRAINTS,
};
pub use schema::{
    gate, gate_and_scan_injections, gate_with_options, injection_markers, parse,
    parse_validator_report, sentence_count, BriefFulfillment, BriefFulfillmentState, DecomposeCase,
    DecomposeOutput, DiversityOutput, GateOptions, ImplementerOutput, JudgeOutput, LaneJudgeReport,
    Role, SchemaError, TextOrList, ValidatorOutput, Variant, Verdict,
};
