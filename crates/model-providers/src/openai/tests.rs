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
