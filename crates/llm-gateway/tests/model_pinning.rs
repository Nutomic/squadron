#![allow(clippy::unwrap_used)] // test code: panics on failure are the assertion mechanism

//! Wave A model-pinning contract: implementer lanes run glm-5.3-flash only,
//! the curl-floor validator (reviewer) runs deepseek-v4.1-flash — a DIFFERENT
//! model than the implementer, so a review is never same-model self-review —
//! and the judge stays glm-5.3.

use llm_gateway::{
    lane_model, SpriteRecipe, DECOMPOSE_MODEL, DIVERSITY_MODEL, IMPLEMENTER_MODEL, JUDGE_MODEL,
    LANE_MODELS, VALIDATOR_MODEL,
};

#[test]
fn reviewer_model_differs_from_implementer_model() {
    assert_ne!(
        IMPLEMENTER_MODEL, VALIDATOR_MODEL,
        "the validator must never be the implementer's own model (same-model self-review)"
    );
}

#[test]
fn pinned_models_have_their_contracted_values() {
    assert_eq!(IMPLEMENTER_MODEL, "glm-5.3-flash");
    assert_eq!(VALIDATOR_MODEL, "deepseek-v4.1-flash");
    assert_eq!(JUDGE_MODEL, "glm-5.3");
    assert_eq!(DIVERSITY_MODEL, "glm-5.3");
    assert_eq!(DECOMPOSE_MODEL, "glm-5.3");
}

#[test]
fn every_implementer_lane_is_pinned_to_the_implementer_model() {
    // The old per-lane alternation is removed: lane_model ignores its index.
    assert_eq!(LANE_MODELS.len(), 1);
    for idx in 0..8 {
        assert_eq!(lane_model(idx), IMPLEMENTER_MODEL, "lane {idx} drifted");
    }
}

#[test]
fn validator_recipe_runs_the_reviewer_model_with_low_effort() {
    let recipe = SpriteRecipe::for_model("run-abc-0-val", VALIDATOR_MODEL);
    assert_eq!(recipe.model(), VALIDATOR_MODEL);
    let request = recipe.build("system", "user");
    assert_eq!(request.model, VALIDATOR_MODEL);
    assert_eq!(request.reasoning_effort.as_deref(), Some("low"));
    assert_ne!(request.model, IMPLEMENTER_MODEL);
}

#[test]
fn implementer_recipe_runs_the_implementer_model_with_low_effort() {
    let recipe = SpriteRecipe::for_model("run-abc-0", IMPLEMENTER_MODEL);
    let request = recipe.build("system", "user");
    assert_eq!(request.model, IMPLEMENTER_MODEL);
    assert_eq!(request.reasoning_effort.as_deref(), Some("low"));
}
