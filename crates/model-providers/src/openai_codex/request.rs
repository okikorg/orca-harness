use orca_harness_core::{Context, Message, ToolSchema};
use serde_json::{json, Value};

pub(crate) fn body(
    model: &str,
    context: &Context,
    tools: &[ToolSchema],
    stream: bool,
    continuation: &[Value],
    reasoning_effort: Option<&str>,
    prompt_cache_key: Option<&str>,
) -> Value {
    let mut instructions = Vec::new();
    let mut input = Vec::new();
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
                input.extend(tool_calls.iter().map(|call| {
                    let mut item = json!({
                        "type": "function_call", "call_id": call.id, "name": call.name
                    });
                    item["arguments"] = Value::String(call.arguments.to_string());
                    item
                }));
            }
            Message::Tool { results } => input.extend(results.iter().map(|result| {
                let mut item = json!({"type": "function_call_output", "call_id": result.call_id});
                item["output"] = Value::String(result.output.to_string());
                item
            })),
        }
    }
    // Encrypted reasoning must immediately precede the tool continuation it
    // belongs to. It is opaque and is never rendered or inserted into Context.
    if let Some(first_tool_output) = input
        .iter()
        .rposition(|item| item["type"] == "function_call_output")
        .filter(|_| !continuation.is_empty())
    {
        let call_id = input[first_tool_output]["call_id"].as_str();
        let mut insertion = input[..first_tool_output]
            .iter()
            .rposition(|item| {
                item["type"] == "function_call" && item["call_id"].as_str() == call_id
            })
            .unwrap_or(first_tool_output);
        while insertion > 0 && input[insertion - 1]["type"] == "function_call" {
            insertion -= 1;
        }
        input.splice(insertion..insertion, continuation.iter().cloned());
    }
    let mut value = json!({
        "model": model,
        "input": [],
        "stream": stream,
        "store": false,
        "reasoning": {"summary": "auto"},
        "include": ["reasoning.encrypted_content"],
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
    if let Some(effort) = reasoning_effort {
        value["reasoning"]["effort"] = json!(effort);
    }
    if let Some(prompt_cache_key) = prompt_cache_key {
        value["prompt_cache_key"] = json!(prompt_cache_key);
    }
    value
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
        let value = body("codex", &context, &tools, true, &[], None, None);
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

        let value = body("codex", &context, &[], true, &[], None, None);
        assert_eq!(
            value["input"][0]["content"],
            json!([
                {"type": "input_text", "text": "describe"},
                {"type": "input_image", "image_url": "data:image/jpeg;base64,/9j/", "detail": "auto"}
            ])
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
            std::slice::from_ref(&reasoning),
            None,
            None,
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
            &[],
            Some("xhigh"),
            Some("run-123"),
        );
        assert_eq!(
            value["reasoning"],
            json!({"effort": "xhigh", "summary": "auto"})
        );
        assert_eq!(value["prompt_cache_key"], "run-123");
    }
}
