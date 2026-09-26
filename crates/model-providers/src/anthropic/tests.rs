use super::{stream::Accumulator, AnthropicModel};
use orca_harness_core::{
    Context, Image, ModelDelta, ModelError, ModelResponse, ToolCall, ToolResult, ToolSchema,
};
use serde_json::{json, Value};

/// No cached thinking blocks: the shape every request has on a first turn.
fn no_thinking() -> std::collections::HashMap<String, std::sync::Arc<[Value]>> {
    std::collections::HashMap::new()
}

fn tools() -> Vec<ToolSchema> {
    vec![ToolSchema {
        name: "lookup".into(),
        description: "Look up a key".into(),
        parameters: json!({"type":"object","properties":{"key":{"type":"string"}}}),
    }]
}

fn shot(data: &str) -> Value {
    json!({"content": "[image 1]", "_images": [{"media_type": "image/png", "data": data}]})
}

#[test]
fn tool_result_images_become_image_blocks_inside_the_tool_result() {
    let calls: Vec<ToolCall> = [("toolu_1", "shot"), ("toolu_2", "list")]
        .iter()
        .map(|(id, name)| ToolCall {
            id: (*id).into(),
            name: (*name).into(),
            arguments: json!({}),
        })
        .collect();
    let mut context = Context::new();
    context.push_user("Look");
    context.push_assistant_tool_calls(None, calls.clone());
    let mixed = json!({
        "content": "before\n[image 1]\nbetween\n[image 2]",
        "_images": [
            {"media_type": "image/png", "data": "iVBO"},
            {"media_type": "image/jpeg", "data": "/9j/"},
        ]
    });
    context.append_tool_results(vec![
        ToolResult::ok(&calls[0], mixed),
        ToolResult::ok(&calls[1], json!("done")),
    ]);
    let body = AnthropicModel::new("claude-test")
        .request_body(&context, &[], &no_thinking())
        .unwrap();
    let results = &body["messages"][2]["content"];
    assert_eq!(
        results[0],
        json!({
            "type": "tool_result", "tool_use_id": "toolu_1", "is_error": false,
            "content": [
                {"type": "text", "text": "before\n[image 1]"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBO"}},
                {"type": "text", "text": "\nbetween\n[image 2]"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "/9j/"}}
            ]
        })
    );
    // A result without images keeps the plain string form.
    assert_eq!(results[1]["content"], "done");
}

#[test]
fn only_the_most_recent_tool_images_are_sent() {
    let max = crate::tool_images::MAX_TOOL_IMAGES;
    let mut context = Context::new();
    context.push_user("Browse");
    for step in 0..max + 3 {
        let call = ToolCall {
            id: format!("toolu_{step}"),
            name: "shot".into(),
            arguments: json!({}),
        };
        context.push_assistant_tool_calls(None, vec![call.clone()]);
        context.append_tool_results(vec![ToolResult::ok(&call, shot(&format!("img{step}")))]);
    }
    let body = AnthropicModel::new("claude-test")
        .request_body(&context, &[], &no_thinking())
        .unwrap();
    let sent: Vec<&str> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().unwrap())
        .filter(|block| block["type"] == "tool_result")
        .flat_map(|block| block["content"].as_array().into_iter().flatten())
        .filter_map(|part| part["source"]["data"].as_str())
        .collect();
    assert_eq!(sent.len(), max);
    assert_eq!(sent[0], "img3");
    // The oldest results fall back to their text, markers included and
    // base64 left out.
    assert_eq!(
        body["messages"][2]["content"][0]["content"],
        "{\"content\":\"[image 1]\"}"
    );
}

#[tokio::test]
async fn native_request_headers_caching_images_and_tool_round_trip() {
    let mut context = Context::new();
    context.push_system("Stable instructions");
    context.push_user_with_images(
        "Inspect",
        vec![Image {
            media_type: "image/png".into(),
            data: "aGVsbG8=".into(),
        }],
    );
    let model = AnthropicModel::new("claude-test")
        .api_key("test-key")
        .prompt_cache(true)
        .max_tokens(1024);
    let initial = model
        .request_body(&context, &tools(), &no_thinking())
        .unwrap();
    let call = ToolCall {
        id: "toolu_1".into(),
        name: "lookup".into(),
        arguments: json!({"key":"a"}),
    };
    context.push_assistant_tool_calls(Some("Checking".into()), vec![call.clone()]);
    context.append_tool_results(vec![ToolResult {
        call_id: call.id,
        tool_name: call.name,
        output: json!({"error":"missing"}),
        is_error: true,
    }]);
    context.push_user("Try again");
    let body = model
        .request_body(&context, &tools(), &no_thinking())
        .unwrap();
    assert_eq!(body["system"], initial["system"]);
    assert_eq!(body["tools"], initial["tools"]);
    assert_eq!(body["messages"][0], initial["messages"][0]);
    assert_eq!(body["tools"][0]["input_schema"], tools()[0].parameters);
    assert_eq!(
        body["messages"][0]["content"][1]["source"],
        json!({"type":"base64","media_type":"image/png","data":"aGVsbG8="})
    );
    assert_eq!(
        body["messages"][1]["content"][1],
        json!({"type":"tool_use","id":"toolu_1","name":"lookup","input":{"key":"a"}})
    );
    assert_eq!(body["messages"][2]["role"], "user");
    assert_eq!(
        body["messages"][2]["content"][0],
        json!({"type":"tool_result","tool_use_id":"toolu_1","content":"{\"error\":\"missing\"}","is_error":true})
    );
    assert_eq!(body["messages"][2]["content"][1]["text"], "Try again");
    assert_eq!(body["cache_control"], json!({"type":"ephemeral"}));
    assert!(body.get("prompt_cache_key").is_none());
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 1024);
    let request = model
        .prepare_request(&context, &tools())
        .await
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(
        request.url().as_str(),
        "https://api.anthropic.com/v1/messages"
    );
    assert_eq!(request.headers()["x-api-key"], "test-key");
    assert!(request.headers()["x-api-key"].is_sensitive());
    assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
    assert!(request.headers().get("authorization").is_none());
    let wire: Value = serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(wire, body);
}

#[tokio::test]
async fn sdk_defaults_custom_endpoint_and_missing_credentials() {
    let mut context = Context::new();
    context.push_user("Hello");
    let model = AnthropicModel::new("claude-test");
    let body = model.request_body(&context, &[], &no_thinking()).unwrap();
    assert!(body.get("cache_control").is_none());
    assert!(body.get("tools").is_none());
    assert!(body.get("system").is_none());
    assert_eq!(body["max_tokens"], 8192);
    // No `thinking` key at all: the only shape every model accepts.
    assert!(body.get("thinking").is_none());
    assert!(matches!(
        model.prepare_request(&context, &[]).await,
        Err(ModelError::Authentication(_))
    ));
    let model = model
        .api_key("key")
        .base_url("https://example.test/v1/")
        .temperature(0.2)
        .reasoning_effort("low");
    let request = model
        .prepare_request(&context, &[])
        .await
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(request.url().as_str(), "https://example.test/v1/messages");
    let body = model.request_body(&context, &[], &no_thinking()).unwrap();
    assert_eq!(body["output_config"]["effort"], "low");
    assert_eq!(body["temperature"], 0.2);
    assert!(AnthropicModel::new("m")
        .max_tokens(0)
        .request_body(&context, &[], &no_thinking())
        .is_err());
}

fn start() -> Value {
    json!({"type":"message_start","message":{"usage":{"input_tokens":11,"output_tokens":1,"cache_creation_input_tokens":20,"cache_read_input_tokens":30}}})
}

fn text_frames() -> Vec<Value> {
    vec![
        start(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Héllo"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}),
        json!({"type":"message_stop"}),
    ]
}

fn collect(events: Vec<Value>) -> Result<ModelResponse, ModelError> {
    let mut acc = Accumulator::default();
    for event in events {
        acc.apply(&event.to_string())?;
    }
    acc.finish().map(|collected| collected.response)
}

#[test]
fn stream_every_byte_split_preserves_utf8_deltas_and_usage() {
    let wire = text_frames()
        .iter()
        .map(|event| {
            format!(
                "event: {}\r\ndata: {event}\r\n\r\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect::<String>();
    for split in 0..=wire.len() {
        let mut frames = crate::sse::SseBuffer::default();
        let mut acc = Accumulator::default();
        let mut text = String::new();
        for chunk in [&wire.as_bytes()[..split], &wire.as_bytes()[split..]] {
            for payload in frames.push(chunk).unwrap() {
                for delta in acc.apply(&payload).unwrap() {
                    if let ModelDelta::Text { text: part } = delta {
                        text.push_str(&part);
                    }
                }
            }
        }
        assert_eq!(text, "Héllo");
        let ModelResponse::Final {
            text,
            usage: Some(usage),
        } = acc.finish().unwrap().response
        else {
            panic!("expected final");
        };
        assert_eq!(text, "Héllo");
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_create_tokens,
                usage.cache_read_tokens
            ),
            (11, 7, 20, 30)
        );
    }
}

fn tool_frames() -> Vec<Value> {
    vec![
        start(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Checking"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"lookup","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"key\":"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a\"}"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_2","name":"lookup","input":{}}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":12}}),
        json!({"type":"message_stop"}),
    ]
}

#[test]
fn parallel_tools_accumulate_json_and_preserve_order() {
    let ModelResponse::ToolCalls {
        calls,
        content,
        usage,
    } = collect(tool_frames()).unwrap()
    else {
        panic!("expected tools");
    };
    assert_eq!(content.as_deref(), Some("Checking"));
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id, "toolu_1");
    assert_eq!(calls[0].arguments, json!({"key":"a"}));
    assert_eq!(calls[1].id, "toolu_2");
    assert_eq!(calls[1].arguments, json!({}));
    assert_eq!(usage.unwrap().output_tokens, 12);
}

#[test]
fn incomplete_invalid_and_limited_output_never_executes_partial_tools() {
    let mut events = tool_frames();
    events.pop();
    assert!(matches!(
        collect(events),
        Err(ModelError::IncompleteResponse { .. })
    ));
    let mut events = tool_frames();
    events[5]["delta"]["partial_json"] = json!("bad");
    assert!(matches!(
        collect(events.clone()),
        Err(ModelError::MalformedToolArguments { .. })
    ));
    events[9]["delta"]["stop_reason"] = json!("max_tokens");
    assert!(matches!(
        collect(events),
        Err(ModelError::OutputLimit { usage: Some(_), .. })
    ));
    let mut events = text_frames();
    events[4]["delta"]["stop_reason"] = json!("refusal");
    assert!(matches!(
        collect(events),
        Err(ModelError::ContentFiltered { .. })
    ));
    let mut events = tool_frames();
    events.remove(6);
    assert!(matches!(
        collect(events),
        Err(ModelError::IncompleteResponse { .. })
    ));
    let mut events = tool_frames();
    events[7]["content_block"]["id"] = json!("toolu_1");
    assert!(matches!(
        collect(events),
        Err(ModelError::InvalidResponse(_))
    ));
}

#[test]
fn stream_errors_keep_retry_classification_and_unknown_events_are_ignored() {
    let mut events = text_frames();
    events.insert(1, json!({"type":"ping"}));
    events.insert(2, json!({"type":"future_event"}));
    assert!(collect(events).is_ok());
    let error = collect(vec![
        json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}}),
    ])
    .unwrap_err();
    assert!(crate::http_error::retry_delay(&error).is_some());
    let error = crate::http_error::request_error(
        429,
        Some("4"),
        r#"{"error":{"type":"rate_limit_error","message":"busy"}}"#,
    );
    assert_eq!(
        crate::http_error::retry_delay(&error),
        Some(std::time::Duration::from_secs(4))
    );
}

#[test]
fn top_level_schema_combinators_are_dropped_and_nested_ones_kept() {
    let nested = json!({"items":{"oneOf":[{"required":["old"]}]}});
    let schema = json!({
        "type":"object","additionalProperties":false,
        "properties":{"action":{"enum":["run","list"]},"edits":nested},
        "required":["action"],
        "oneOf":[{"required":["graph"]}],
        "anyOf":[{"required":["runId"]}],
        "allOf":[{"required":["stage"]}]
    });
    let model = AnthropicModel::new("claude-test").api_key("test-key");
    let body = model
        .request_body(
            &{
                let mut context = Context::new();
                context.push_user("go");
                context
            },
            &[ToolSchema {
                name: "workflow".into(),
                description: "Run a graph".into(),
                parameters: schema,
            }],
            &no_thinking(),
        )
        .unwrap();
    let sent = &body["tools"][0]["input_schema"];
    for keyword in ["oneOf", "anyOf", "allOf"] {
        assert!(
            sent.get(keyword).is_none(),
            "{keyword} survived at top level"
        );
    }
    assert_eq!(sent["properties"]["edits"], nested);
    assert_eq!(sent["required"], json!(["action"]));
    assert_eq!(sent["additionalProperties"], json!(false));
}

#[test]
fn thinking_blocks_stream_as_reasoning_and_stay_out_of_the_answer() {
    let mut accumulator = Accumulator::default();
    let events = [
        json!({"type":"message_start","message":{"usage":{"input_tokens":9}}}),
        json!({"type":"content_block_start","index":0,
            "content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,
            "delta":{"type":"thinking_delta","thinking":"weigh"}}),
        json!({"type":"content_block_delta","index":0,
            "delta":{"type":"signature_delta","signature":"sig-a"}}),
        json!({"type":"content_block_delta","index":0,
            "delta":{"type":"signature_delta","signature":"sig-b"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,
            "content_block":{"type":"redacted_thinking","data":"opaque"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":2,
            "delta":{"type":"text_delta","text":"the answer"}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        json!({"type":"message_stop"}),
    ];
    let mut reasoning = String::new();
    for event in events {
        for delta in accumulator.apply(&event.to_string()).unwrap() {
            match delta {
                ModelDelta::Reasoning { text } => reasoning.push_str(&text),
                ModelDelta::Text { text } => assert_eq!(text, "the answer"),
                ModelDelta::ToolInput { .. } => panic!("no tool call was streamed"),
            }
        }
    }
    assert_eq!(reasoning, "weigh");
    // Reasoning is presentation-only; the answer never absorbs it.
    let collected = accumulator.finish().unwrap();
    // Both blocks are kept for replay, in the order the model produced them.
    assert_eq!(collected.thinking.len(), 2);
    assert_eq!(collected.thinking[0]["signature"], "sig-asig-b");
    assert_eq!(collected.thinking[0]["thinking"], "weigh");
    assert_eq!(collected.thinking[1]["data"], "opaque");
    match collected.response {
        ModelResponse::Final { text, .. } => assert_eq!(text, "the answer"),
        other => panic!("expected a final answer, got {other:?}"),
    }
}

#[test]
fn replayed_thinking_leads_its_own_assistant_turn() {
    let thinking = json!({"type":"thinking","thinking":"weigh","signature":"sig"});
    let call = ToolCall {
        id: "toolu_9".into(),
        name: "lookup".into(),
        arguments: json!({"key":"a"}),
    };
    let mut cached = std::collections::HashMap::new();
    cached.insert(
        call.id.clone(),
        std::sync::Arc::from(vec![thinking.clone()]),
    );

    let mut context = Context::new();
    context.push_user("Look it up");
    context.push_assistant_tool_calls(Some("Checking".into()), vec![call.clone()]);
    context.append_tool_results(vec![ToolResult {
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        output: json!("found"),
        is_error: false,
    }]);
    let model = AnthropicModel::new("claude-test").api_key("test-key");
    let body = model.request_body(&context, &tools(), &cached).unwrap();

    let assistant = &body["messages"][1];
    assert_eq!(assistant["role"], "assistant");
    // Thinking leads, then text, then the tool_use it justifies.
    assert_eq!(assistant["content"][0], thinking);
    assert_eq!(assistant["content"][1]["type"], "text");
    assert_eq!(assistant["content"][2]["id"], "toolu_9");
    // Blocks cached under a different turn's call ids stay out of this turn:
    // lookup is keyed per assistant turn, never "the most recent blocks win".
    let mut other = std::collections::HashMap::new();
    other.insert(
        "toolu_other".to_owned(),
        std::sync::Arc::from(vec![json!({"type":"thinking","thinking":"elsewhere"})]),
    );
    let body = model.request_body(&context, &tools(), &other).unwrap();
    assert_eq!(body["messages"][1]["content"][0]["type"], "text");
    let body = model
        .request_body(&context, &tools(), &no_thinking())
        .unwrap();
    assert_eq!(body["messages"][1]["content"][0]["type"], "text");
}
