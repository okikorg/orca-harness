use orca_harness_core::{Context, Message, ToolSchema};
use serde_json::{json, Value};

/// Request options that differ between Codex and generic Responses services.
#[derive(Clone, Copy, Default)]
pub(crate) struct Options<'a> {
    pub(crate) reasoning_effort: Option<&'a str>,
    pub(crate) prompt_cache_key: Option<&'a str>,
    /// Send a reasoning configuration without an effort. Codex always
    /// reasons; generic Responses also serves non-reasoning models, which
    /// reject one.
    pub(crate) always_reason: bool,
    /// Ask for encrypted reasoning, an OpenAI-specific include selector.
    pub(crate) encrypted_reasoning: bool,
}

impl<'a> Options<'a> {
    /// Codex always reasons and replays encrypted reasoning.
    pub(crate) fn codex(
        reasoning_effort: Option<&'a str>,
        prompt_cache_key: Option<&'a str>,
    ) -> Self {
        Self {
            reasoning_effort,
            prompt_cache_key,
            always_reason: true,
            encrypted_reasoning: true,
        }
    }
}

/// `reasoning` maps a tool call id to the encrypted reasoning items that
/// produced it; they are replayed immediately before that assistant's calls.
pub(crate) fn body<'r>(
    model: &str,
    context: &Context,
    tools: &[ToolSchema],
    stream: bool,
    options: Options<'_>,
    reasoning: impl Fn(&str) -> Option<&'r [Value]>,
) -> Value {
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    let mut recent = crate::tool_images::Recent::new(context);
    for message in context.messages() {
        match message {
            Message::System { content } => instructions.push(content.as_str()),
            Message::User { content, images } => {
                let mut parts = vec![json!({"type": "input_text", "text": content})];
                parts.extend(images.iter().map(|image| {
                    let mut part = json!({"type": "input_image", "detail": "auto"});
                    part["image_url"] = Value::String(crate::image_data_url(image));
                    part
                }));
                let mut message = json!({
                    "type": "message", "role": "user", "content": null
                });
                message["content"] = Value::Array(parts);
                input.push(message);
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                if let Some(text) = content {
                    input.push(json!({
                        "type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": text}]
                    }));
                }
                // Encrypted reasoning must immediately precede the calls it
                // produced. It is opaque and is never rendered or inserted
                // into Context. All calls from one response share it.
                if let Some(items) = tool_calls.iter().find_map(|call| reasoning(&call.id)) {
                    input.extend(items.iter().cloned());
                }
                input.extend(tool_calls.iter().map(|call| {
                    let mut item = json!({
                        "type": "function_call", "call_id": call.id, "name": call.name
                    });
                    item["arguments"] = Value::String(call.arguments.to_string());
                    item
                }));
            }
            Message::Tool { results } => input.extend(results.iter().map(|result| {
                let (output, images) = crate::tool_images::split(&result.output);
                let mut item = json!({"type": "function_call_output", "call_id": result.call_id});
                item["output"] = Value::String(output.to_string());
                // The output may instead be a list of input items, which
                // carries images inside the call's own output.
                let images = recent.fresh(images);
                if !images.is_empty() {
                    let text = crate::tool_images::text_of(&output);
                    let parts = crate::tool_images::interleave(&text, images)
                        .into_iter()
                        .map(|part| match part {
                            crate::tool_images::Part::Text(text) => {
                                json!({"type": "input_text", "text": text})
                            }
                            crate::tool_images::Part::Image(image) => {
                                let mut part = json!({"type": "input_image", "detail": "auto"});
                                part["image_url"] = Value::String(image.data_url());
                                part
                            }
                        })
                        .collect();
                    item["output"] = Value::Array(parts);
                }
                item
            })),
        }
    }
    let mut value = json!({
        "model": model,
        "input": [],
        "stream": stream,
        "store": false,
        "parallel_tool_calls": true
    });
    value["instructions"] = Value::String(instructions.join("\n\n"));
    value["input"] = Value::Array(input);
    if tools.is_empty() {
        value.as_object_mut().unwrap().remove("parallel_tool_calls");
    } else {
        value["tools"] = Value::Array(
            tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function", "name": tool.name,
                        "description": tool.description, "parameters": tool.parameters,
                        "strict": false
                    })
                })
                .collect(),
        );
    }
    if options.always_reason || options.reasoning_effort.is_some() {
        value["reasoning"] = json!({"summary": "auto"});
    }
    if let Some(effort) = options.reasoning_effort {
        value["reasoning"]["effort"] = json!(effort);
    }
    if options.encrypted_reasoning {
        value["include"] = json!(["reasoning.encrypted_content"]);
    }
    if let Some(prompt_cache_key) = options.prompt_cache_key {
        value["prompt_cache_key"] = json!(prompt_cache_key);
    }
    value
}

/// Codex replays one continuation: the reasoning for the calls answered by
/// the context's final tool result. Earlier tool turns get none.
pub(crate) fn latest_turn<'a>(
    context: &'a Context,
    continuation: &'a [Value],
) -> impl Fn(&str) -> Option<&'a [Value]> + 'a {
    let last = context
        .messages()
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Tool { results } => results.last().map(|result| result.call_id.as_str()),
            _ => None,
        });
    move |call_id| (!continuation.is_empty() && last == Some(call_id)).then_some(continuation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_harness_core::{Image, ToolCall, ToolResult};

    #[test]
    fn maps_context_and_tools_to_responses_items() {
        let call = ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: json!({"cmd":"pwd"}),
        };
        let mut context = Context::new();
        context.push_system("be careful");
        context.push_user("where am I?");
        context.push_assistant_tool_calls(None, vec![call.clone()]);
        context.append_tool_results(vec![ToolResult::ok(&call, json!({"stdout":"/tmp"}))]);
        let tools = [ToolSchema {
            name: "shell".into(),
            description: "run".into(),
            parameters: json!({"type":"object"}),
        }];
        let value = body(
            "codex",
            &context,
            &tools,
            true,
            Options::codex(None, None),
            |_| None,
        );
        assert_eq!(value["instructions"], "be careful");
        assert_eq!(value["input"][1]["type"], "function_call");
        assert_eq!(value["input"][2]["type"], "function_call_output");
        assert_eq!(value["tools"][0]["name"], "shell");
        assert_eq!(value["store"], false);
        assert_eq!(value["reasoning"], json!({"summary": "auto"}));
        assert_eq!(value["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn user_images_append_data_url_parts() {
        let mut context = Context::new();
        context.push_user_with_images(
            "describe",
            vec![Image {
                media_type: "image/jpeg".into(),
                data: "/9j/".into(),
            }],
        );

        let value = body(
            "codex",
            &context,
            &[],
            true,
            Options::codex(None, None),
            |_| None,
        );
        assert_eq!(
            value["input"][0]["content"],
            json!([
                {"type": "input_text", "text": "describe"},
                {"type": "input_image", "image_url": "data:image/jpeg;base64,/9j/", "detail": "auto"}
            ])
        );
    }

    #[test]
    fn tool_result_images_become_input_images_in_the_call_output() {
        let call = ToolCall {
            id: "c1".into(),
            name: "shot".into(),
            arguments: json!({}),
        };
        let mut context = Context::new();
        context.push_assistant_tool_calls(None, vec![call.clone()]);
        context.append_tool_results(vec![ToolResult::ok(
            &call,
            json!({"content": "[image 1]", "_images": [{"media_type": "image/png", "data": "iVBO"}]}),
        )]);

        let value = body(
            "codex",
            &context,
            &[],
            true,
            Options::codex(None, None),
            |_| None,
        );
        assert_eq!(
            value["input"][1],
            json!({
                "type": "function_call_output", "call_id": "c1",
                "output": [
                    {"type": "input_text", "text": "[image 1]"},
                    {"type": "input_image", "image_url": "data:image/png;base64,iVBO", "detail": "auto"}
                ]
            })
        );
    }

    #[test]
    fn encrypted_reasoning_replays_before_tool_output() {
        let call = ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: json!({"cmd":"pwd"}),
        };
        let mut context = Context::new();
        context.push_assistant_tool_calls(None, vec![call.clone()]);
        context.append_tool_results(vec![ToolResult::ok(&call, json!({"ok":true}))]);
        let reasoning = json!({
            "type":"reasoning", "id":"r1", "summary":[], "encrypted_content":"opaque"
        });
        let value = body(
            "codex",
            &context,
            &[],
            true,
            Options::codex(None, None),
            latest_turn(&context, std::slice::from_ref(&reasoning)),
        );
        let input = value["input"].as_array().unwrap();
        let reasoning_at = input
            .iter()
            .position(|item| item["type"] == "reasoning")
            .unwrap();
        let call_at = input
            .iter()
            .position(|item| item["type"] == "function_call")
            .unwrap();
        let output_at = input
            .iter()
            .position(|item| item["type"] == "function_call_output")
            .unwrap();
        assert!(reasoning_at < call_at && call_at < output_at);
        assert_eq!(input[reasoning_at]["encrypted_content"], "opaque");
    }

    #[test]
    fn selected_effort_uses_responses_reasoning_shape() {
        let value = body(
            "codex",
            &Context::new(),
            &[],
            true,
            Options::codex(Some("xhigh"), Some("run-123")),
            |_| None,
        );
        assert_eq!(
            value["reasoning"],
            json!({"effort": "xhigh", "summary": "auto"})
        );
        assert_eq!(value["prompt_cache_key"], "run-123");
    }

    #[test]
    fn replay_covers_every_mapped_turn_or_only_the_latest_for_codex() {
        let mut context = Context::new();
        context.push_user("start");
        for turn in 0..2 {
            let call = ToolCall {
                id: format!("c{turn}"),
                name: "shell".into(),
                arguments: json!({}),
            };
            context.push_assistant_tool_calls(Some(format!("turn {turn}")), vec![call.clone()]);
            context.append_tool_results(vec![ToolResult::ok(&call, json!({"ok":true}))]);
        }
        let reasoning =
            |turn: usize| vec![json!({"type":"reasoning", "encrypted_content":format!("r{turn}")})];
        let (first, second) = (reasoning(0), reasoning(1));
        let kinds = |value: Value| -> Vec<String> {
            value["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| match item["type"].as_str().unwrap() {
                    "reasoning" => item["encrypted_content"].as_str().unwrap().to_string(),
                    kind => kind.to_string(),
                })
                .collect()
        };
        // Full-context Responses replays each turn before its own calls.
        let every = body(
            "m",
            &context,
            &[],
            true,
            Options::default(),
            |id| match id {
                "c0" => Some(&first[..]),
                "c1" => Some(&second[..]),
                _ => None,
            },
        );
        assert_eq!(
            kinds(every),
            [
                "message",
                "message",
                "r0",
                "function_call",
                "function_call_output",
                "message",
                "r1",
                "function_call",
                "function_call_output"
            ]
        );
        // Codex replays only the continuation of the latest tool turn.
        let latest = body(
            "m",
            &context,
            &[],
            true,
            Options::codex(None, None),
            latest_turn(&context, &second),
        );
        assert_eq!(
            kinds(latest),
            [
                "message",
                "message",
                "function_call",
                "function_call_output",
                "message",
                "r1",
                "function_call",
                "function_call_output"
            ]
        );
    }
}
