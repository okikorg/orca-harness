use super::*;
use orca_harness_core::Image;

fn schemas() -> Vec<ToolSchema> {
    vec![ToolSchema {
        name: "grep".into(),
        description: "search".into(),
        parameters: json!({"type": "object"}),
    }]
}

fn context() -> Context {
    let mut context = Context::new();
    context.push_user("hi");
    context
}

#[test]
fn text_only_user_content_remains_a_string() {
    let messages = encode_messages(&context());
    assert_eq!(messages[0]["content"], "hi");
}

#[test]
fn user_images_follow_text_as_data_urls() {
    let mut context = Context::new();
    context.push_user_with_images(
        "describe",
        vec![Image {
            media_type: "image/png".into(),
            data: "aGVsbG8=".into(),
        }],
    );

    let messages = encode_messages(&context);
    assert_eq!(
        messages[0]["content"][0],
        json!({"type": "text", "text": "describe"})
    );
    assert_eq!(
        messages[0]["content"][1],
        json!({
            "type": "image_url",
            "image_url": {"url": "data:image/png;base64,aGVsbG8="}
        })
    );
}

#[test]
fn parallel_tool_calls_is_omitted_when_unset() {
    let model = OpenAiModel::new("m");
    let body = model.request_body(&context(), &schemas());
    assert!(body.get("parallel_tool_calls").is_none());
}

#[test]
fn parallel_tool_calls_is_sent_when_set() {
    let model = OpenAiModel::new("m").parallel_tool_calls(true);
    let body = model.request_body(&context(), &schemas());
    assert_eq!(body["parallel_tool_calls"], json!(true));

    let model = OpenAiModel::new("m").parallel_tool_calls(false);
    let body = model.request_body(&context(), &schemas());
    assert_eq!(body["parallel_tool_calls"], json!(false));
}

#[test]
fn parallel_tool_calls_is_omitted_without_tools() {
    // OpenAI rejects the field on tool-less requests.
    let model = OpenAiModel::new("m").parallel_tool_calls(true);
    let body = model.request_body(&context(), &[]);
    assert!(body.get("parallel_tool_calls").is_none());
}

#[test]
fn reasoning_effort_uses_openai_chat_completions_shape() {
    let model = OpenAiModel::new("m").reasoning_effort("high");
    let body = model.request_body(&context(), &[]);
    assert_eq!(body["reasoning_effort"], "high");
    assert!(body.get("reasoning").is_none());
}

#[test]
fn nested_reasoning_effort_uses_openrouter_shape() {
    let model = OpenAiModel::new("m").nested_reasoning_effort("low");
    let body = model.request_body(&context(), &[]);
    assert_eq!(body["reasoning"], json!({"effort": "low"}));
    assert!(body.get("reasoning_effort").is_none());
}

#[test]
fn prompt_cache_and_session_are_opt_in() {
    let plain = OpenAiModel::new("m").request_body(&context(), &[]);
    assert!(plain.get("cache_control").is_none());
    assert!(plain.get("session_id").is_none());
    assert!(plain.get("prompt_cache_key").is_none());

    let cached = OpenAiModel::new("m")
        .prompt_cache(true)
        .session_id("run-123")
        .request_body(&context(), &[]);
    assert_eq!(cached["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(cached["session_id"], "run-123");

    let openai = OpenAiModel::new("m")
        .prompt_cache_key("run-456")
        .request_body(&context(), &[]);
    assert_eq!(openai["prompt_cache_key"], "run-456");
    assert!(openai.get("cache_control").is_none());
}

// Prompt caching only pays off when the front of the request is byte-stable
// across the turns of one session: the provider matches a prefix, so a single
// byte that moves turn to turn writes a fresh entry instead of reading the
// previous one. That failure is invisible in output — the answers stay
// correct, the bill goes up — so it is asserted here rather than left to the
// live harness-comparison run, which needs an API key and cannot gate a PR.
#[test]
fn the_cached_request_prefix_is_byte_stable_across_turns() {
    let model = OpenAiModel::new("m").prompt_cache(true);
    let prefix = |context: &Context| {
        let body = model.request_body(context, &schemas());
        serde_json::to_string(&json!({
            "model": body["model"],
            "system": body["messages"][0],
            "tools": body["tools"],
        }))
        .unwrap()
    };

    let mut context = Context::new();
    context.push_system("you are a coding agent");
    context.push_user("first question");
    let turn_one = prefix(&context);

    context.push_assistant_text("an answer");
    context.push_user("second question");
    let turn_two = prefix(&context);

    context.push_assistant_text("another answer");
    context.push_user("third question");
    let turn_three = prefix(&context);

    assert_eq!(turn_one, turn_two);
    assert_eq!(turn_two, turn_three);
}

// Registration order is what makes the tool half of that prefix stable; a
// registry that iterated a hash map would reshuffle it per process and cost
// the cache on every first turn.
#[test]
fn tool_order_survives_into_the_request_verbatim() {
    let tools: Vec<ToolSchema> = ["read_file", "list_dir", "grep", "glob"]
        .iter()
        .map(|name| ToolSchema {
            name: (*name).into(),
            description: "t".into(),
            parameters: json!({"type": "object"}),
        })
        .collect();

    let body = OpenAiModel::new("m").request_body(&context(), &tools);
    let sent: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();

    assert_eq!(sent, ["read_file", "list_dir", "grep", "glob"]);
}
