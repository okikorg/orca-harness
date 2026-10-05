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
            source_url: None,
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
fn tool_result_images_follow_the_tool_batch_in_one_user_message() {
    use orca_harness_core::{ToolCall, ToolResult};
    let calls: Vec<ToolCall> = ["c1", "c2", "c3"]
        .iter()
        .map(|id| ToolCall {
            id: (*id).into(),
            name: "shot".into(),
            arguments: json!({}),
        })
        .collect();
    let image = |data: &str| json!({"media_type": "image/png", "data": data});
    let mut context = context();
    context.push_assistant_tool_calls(None, calls.clone());
    context.append_tool_results(vec![
        ToolResult::ok(
            &calls[0],
            json!({"content": "[image 1]", "_images": [image("AAAA")]}),
        ),
        ToolResult::ok(&calls[1], json!("ok")),
        ToolResult::ok(
            &calls[2],
            json!({"content": "[image 1]\n[image 2]", "_images": [image("BBBB"), image("CCCC")]}),
        ),
    ]);

    let messages = encode_messages(&context);
    let roles: Vec<&str> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "tool", "tool", "tool", "user"]);
    assert_eq!(messages[2]["content"], "{\"content\":\"[image 1]\"}");
    let url = |data: &str| json!({"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{data}")}});
    assert_eq!(
        messages[5]["content"],
        json!([
            {"type": "text", "text": "[image 1] from tool call c1 (shot):"},
            url("AAAA"),
            {"type": "text", "text": "[image 1] from tool call c3 (shot):"},
            url("BBBB"),
            {"type": "text", "text": "[image 2] from tool call c3 (shot):"},
            url("CCCC"),
        ])
    );

    // No images, no extra message.
    let mut plain = self::context();
    plain.push_assistant_tool_calls(None, vec![calls[1].clone()]);
    plain.append_tool_results(vec![ToolResult::ok(&calls[1], json!("ok"))]);
    assert_eq!(encode_messages(&plain).len(), 3);
}

#[test]
fn top_level_combinators_are_stripped_from_tool_parameters() {
    let tools = vec![ToolSchema {
        name: "workflow".into(),
        description: "run".into(),
        parameters: json!({
            "type": "object",
            "properties": {"action": {"enum": ["run", "list"]}},
            "required": ["action"],
            "oneOf": [{"properties": {"action": {"const": "run"}}}],
        }),
    }];
    let body = OpenAiModel::new("m").request_body(&context(), &tools);
    let sent = &body["tools"][0]["function"]["parameters"];
    for keyword in ["oneOf", "anyOf", "allOf"] {
        assert!(sent.get(keyword).is_none(), "sent a top-level `{keyword}`");
    }
    assert_eq!(sent["properties"], tools[0].parameters["properties"]);
    assert_eq!(sent["required"], json!(["action"]));
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

#[test]
fn completion_reasoning_tokens_preserve_absent_zero_and_nonzero() {
    for (details, expected) in [
        (json!({}), None),
        (json!({"reasoning_tokens": 0}), Some(0)),
        (json!({"reasoning_tokens": 21}), Some(21)),
    ] {
        let wire: WireUsage = serde_json::from_value(json!({
            "prompt_tokens": 10,
            "completion_tokens": 30,
            "completion_tokens_details": details,
        }))
        .unwrap();
        let usage = wire.into_usage();

        assert_eq!(usage.output_tokens, 30);
        assert_eq!(usage.reasoning_tokens, expected);
    }
}

#[test]
fn model_options_are_opt_in_and_do_not_enable_router_usage() {
    let body = OpenAiModel::new("m").request_body(&context(), &[]);
    for key in [
        "service_tier",
        "verbosity",
        "response_format",
        "usage",
        "text",
        "reasoning",
    ] {
        assert!(body.get(key).is_none(), "{key}");
    }
    let model = OpenAiModel::new("m")
        .service_tier("priority")
        .verbosity("low")
        .reasoning_effort("high")
        .text_format(json!({"type":"text"}))
        .unwrap();
    let body = model.request_body(&context(), &schemas());
    assert_eq!(body["service_tier"], "priority");
    assert_eq!(body["verbosity"], "low");
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["response_format"], json!({"type":"text"}));
    for key in ["usage", "text", "reasoning", "summary"] {
        assert!(body.get(key).is_none(), "{key}");
    }
}

#[test]
fn text_formats_translate_to_chat_without_losing_schema_attributes() {
    for kind in ["text", "json_object"] {
        let body = OpenAiModel::new("m")
            .text_format(json!({"type":kind}))
            .unwrap()
            .request_body(&context(), &[]);
        assert_eq!(body["response_format"], json!({"type":kind}));
    }
    let attributes = json!({"name":"answer_v1", "description":"An answer", "schema":{"type":"object", "properties":{"answer":{"type":"string"}}, "required":["answer"], "additionalProperties":false}, "strict":true});
    let mut input = attributes.clone();
    input["type"] = json!("json_schema");
    let body = OpenAiModel::new("m")
        .text_format(input)
        .unwrap()
        .request_body(&context(), &[]);
    assert_eq!(
        body["response_format"],
        json!({"type":"json_schema", "json_schema":attributes})
    );
    assert!(body.get("schema").is_none());
    assert!(body.get("text").is_none());
}

#[test]
fn chat_response_format_can_be_supplied_directly() {
    let format = json!({"type":"json_schema", "json_schema":{"name":"answer", "schema":{"type":"object"}, "strict":false}});
    let body = OpenAiModel::new("m")
        .response_format(format.clone())
        .request_body(&context(), &[]);
    assert_eq!(body["response_format"], format);
}

#[test]
fn unsupported_or_malformed_text_formats_are_rejected() {
    for input in [
        Value::Null,
        json!({}),
        json!({"type":"grammar"}),
        json!({"type":"text", "schema":{}}),
        json!({"type":"json_schema", "name":"valid", "schema":{}, "extra":true}),
        json!({"type":"json_schema", "name":"bad name", "schema":{}}),
        json!({"type":"json_schema", "name":"valid", "schema":[]}),
        json!({"type":"json_schema", "name":"valid", "schema":{}, "strict":"yes"}),
        json!({"type":"json_schema", "name":"valid", "schema":{}, "description":1}),
    ] {
        // Not `Request`: the retry layer would resend a configuration error.
        assert!(
            matches!(
                OpenAiModel::new("m").text_format(input.clone()),
                Err(ModelError::InvalidResponse(_))
            ),
            "{input}"
        );
    }
}

#[test]
fn user_image_urls_are_native_image_parts() {
    let mut context = Context::new();
    context.push_user_with_images(
        "describe",
        vec![Image::url("https://images.example.test/pixel.png").unwrap()],
    );
    let messages = encode_messages(&context);
    assert_eq!(
        messages[0]["content"][1],
        json!({"type":"image_url","image_url":{"url":"https://images.example.test/pixel.png"}})
    );
}

#[test]
fn an_image_with_bytes_and_a_url_is_sent_inline() {
    let mut context = Context::new();
    let mut image = Image::base64("image/png", "aGVsbG8=");
    image.source_url = Some("https://images.example.test/pixel.png".into());
    context.push_user_with_images("describe", vec![image]);
    let messages = encode_messages(&context);
    assert_eq!(
        messages[0]["content"][1]["image_url"]["url"],
        "data:image/png;base64,aGVsbG8="
    );
}
