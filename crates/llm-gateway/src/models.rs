//! Model pinning — the single source of truth for which LLM runs which role.
//!
//! Pinning rules (Wave A):
//! - Every implementer lane runs the SAME model; interpretation diversity
//!   comes from prompt-level variant generation, not from alternating lane
//!   models.
//! - The validator (curl-floor reviewer) MUST be a different model than the
//!   implementer: same-model self-review is a rubber stamp.
//! - The judge runs glm-5.3-flash with screenshots (image-capable,
//!   probe-verified 2026-11-25), then a different-family deepseek-v4.1-flash
//!   text-only fallback so a gateway/model failure cannot kill a run.

/// Implementer lanes: glm-5.3-flash only (lane alternation removed).
pub const IMPLEMENTER_MODEL: &str = "glm-5.3-flash";

/// Curl-floor validator (the reviewer). Deliberately a different family than
/// IMPLEMENTER_MODEL so the review is never same-model self-review.
/// NOTE: deepseek is served through the opencode.ai console, which requires
/// the privacy region set to **Global** for this model. That is already
/// configured in the console — if lane validator sessions start failing with
/// model-not-found errors, check the console privacy region first.
pub const VALIDATOR_MODEL: &str = "deepseek-v4.1-flash";

/// Machine judge: glm-5.3-flash WITH screenshots. Live probe 2026-11-25:
/// the identical image parts that glm-5.3 rejects with HTTP 400
/// `[invalid_request]` (first seen 2026-11-24, reconfirmed 2026-11-25) returned
/// 200 in 3.7s with coherent ranked output. `reasoning_effort` stays low.
pub const JUDGE_MODEL: &str = "glm-5.3-flash";

/// Judge text-only fallback: deepseek-v4.1-flash, a DIFFERENT model family so
/// it is a different failure domain from the flash primary. Live probe
/// 2026-11-25: text-only requests succeed while image requests 400, hence this
/// rung carries no image parts.
pub const JUDGE_TEXT_FALLBACK_MODEL: &str = "deepseek-v4.1-flash";

/// Machine-side diversity generator (prompt-level diversity stays).
pub const DIVERSITY_MODEL: &str = "glm-5.3";

/// Wave B: machine-side brief decomposition (lane count + per-case implementer
/// prompts). Planner is a simple splitting task; glm-5.3-flash is probe-verified
/// 2× faster than glm-5.3 at equal quality (reasoning_effort stays low).
pub const DECOMPOSE_MODEL: &str = "glm-5.3-flash";

/// Alternation removed: every implementer lane runs the pinned model. Kept as
/// a one-element list so `lane_model` call sites stay valid.
pub const LANE_MODELS: [&str; 1] = [IMPLEMENTER_MODEL];

/// Implementer model for lane `index`. The index is ignored: all lanes are
/// pinned to [`IMPLEMENTER_MODEL`] (the old glm-5.3-flash / deepseek
/// alternation is gone).
pub fn lane_model(_index: usize) -> &'static str {
    IMPLEMENTER_MODEL
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wave A: the reviewer must be a different model than the implementer.
    #[test]
    fn validator_model_differs_from_implementer_model() {
        assert_ne!(
            IMPLEMENTER_MODEL, VALIDATOR_MODEL,
            "same-model self-review is a rubber stamp"
        );
    }
}
