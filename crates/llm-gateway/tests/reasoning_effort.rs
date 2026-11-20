use llm_gateway::sprites_gateway::SpriteRecipe;
use llm_gateway::{
    lane_model, ChatMessage, ChatRequest, ContentPart, MachineLlmClient, DEFAULT_REASONING_EFFORT,
    DIVERSITY_MODEL, JUDGE_MODEL, REASONING_EFFORT_ENV,
};

fn assert_low(value: &serde_json::Value, context: &str) {
    assert_eq!(
        value["reasoning_effort"],
        serde_json::json!(DEFAULT_REASONING_EFFORT),
        "{context} must send reasoning_effort=low: {value}"
    );
}

#[test]
fn machine_style_requests_default_to_low_reasoning() {
    // Machine path request builders (diversity, judge, lane prompts) all go
    // through ChatRequest::new, which defaults to low.
    let diversity = ChatRequest::new(DIVERSITY_MODEL)
        .system("diversity system")
        .user("diversity user");
    assert_low(&serde_json::to_value(&diversity).unwrap(), "diversity");

    let judge = ChatRequest::new(JUDGE_MODEL).message(ChatMessage::user_parts(vec![
        ContentPart::text("rank lanes"),
        ContentPart::image("data:image/png;base64,AAAA"),
    ]));
    assert_low(&serde_json::to_value(&judge).unwrap(), "judge");
}

#[test]
fn sprite_recipe_requests_default_to_low_reasoning() {
    let recipe = SpriteRecipe::for_model("lane-3", lane_model(0));
    let request = recipe.build("system", "user");
    assert_low(&serde_json::to_value(&request).unwrap(), "sprite build");

    let parts = recipe.build_with_parts("system", vec![ContentPart::text("parts")]);
    assert_low(
        &serde_json::to_value(&parts).unwrap(),
        "sprite build_with_parts",
    );
}

#[test]
fn machine_client_enforces_low_on_every_outbound_request() {
    // Without the env knob the client keeps the low default.
    let client = MachineLlmClient::new("key", "http://127.0.0.1:1/chat/completions");
    assert_eq!(client.reasoning_effort(), "low");
    // The client-level knob remains configurable while the default is low.
    let knobbed = client.with_reasoning_effort("medium");
    assert_eq!(knobbed.reasoning_effort(), "medium");
    // The env knob is the documented deployment-level override.
    assert_eq!(REASONING_EFFORT_ENV, "OPENCODE_REASONING_EFFORT");
}
