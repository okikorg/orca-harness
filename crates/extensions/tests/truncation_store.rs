//! The Truncation + TruncationStore + read_tool_result pair, exercised
//! directly through the extension hook and the tool.

use serde_json::{json, Value};

use orca_harness_core::{CancellationToken, Extension, Tool, ToolCall, ToolContext, ToolResult};
use orca_harness_extensions::{ReadToolResultTool, Truncation, TruncationStore};

fn ctx() -> ToolContext {
    ToolContext {
        call_id: "t".into(),
        tool_name: "t".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({}),
    }
}

fn result(call: &ToolCall, output: Value) -> ToolResult {
    ToolResult::ok(call, output)
}

#[tokio::test]
async fn truncated_output_is_stored_and_pageable() {
    let store = TruncationStore::default();
    let trunc = Truncation::new(100).store(store.clone());
    let reader = ReadToolResultTool::new(store);

    let big: String = "x".repeat(1000);
    let c = call("call-1", "read_file");
    let original = json!({ "content": big });
    let expected_full = serde_json::to_string(&original).unwrap();

    let out = trunc.after_tool(&c, result(&c, original)).await.unwrap();
    assert_eq!(out.output["_truncated"], json!(true));
    let hint = out.output["_readFull"].as_str().unwrap();
    assert!(hint.contains("call-1"), "hint names the call id: {hint}");
    assert!(out.output["content"].as_str().unwrap().len() < 1000);

    // Page the original back out and reassemble it exactly.
    let mut reassembled = String::new();
    let mut offset = 0u64;
    loop {
        let page = reader
            .call(
                json!({"callId": "call-1", "offset": offset, "maxChars": 300}),
                &ctx(),
            )
            .await
            .unwrap();
        assert_eq!(page["toolName"], json!("read_file"));
        reassembled.push_str(page["content"].as_str().unwrap());
        if !page["hasMore"].as_bool().unwrap() {
            break;
        }
        offset = page["nextOffset"].as_u64().unwrap();
    }
    assert_eq!(reassembled, expected_full);
}

#[tokio::test]
async fn small_outputs_are_not_stored() {
    let store = TruncationStore::default();
    let trunc = Truncation::new(100).store(store.clone());
    let reader = ReadToolResultTool::new(store);

    let c = call("call-2", "shell");
    let out = trunc
        .after_tool(&c, result(&c, json!({"stdout": "short"})))
        .await
        .unwrap();
    assert!(out.output.get("_truncated").is_none());
    let err = reader
        .call(json!({"callId": "call-2"}), &ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no stored output"));
}

#[tokio::test]
async fn reader_output_is_exempt_from_truncation() {
    let store = TruncationStore::default();
    let trunc = Truncation::new(100).store(store.clone());

    let c = call("call-3", "read_tool_result");
    let big: String = "y".repeat(500);
    let out = trunc
        .after_tool(&c, result(&c, json!({"content": big.clone()})))
        .await
        .unwrap();
    assert_eq!(out.output["content"], json!(big), "slice must pass through");
    assert!(out.output.get("_truncated").is_none());
}

#[tokio::test]
async fn store_evicts_oldest_when_over_budget() {
    // Budget fits one large entry, not two.
    let store = TruncationStore::new(2000);
    let trunc = Truncation::new(100).store(store.clone());
    let reader = ReadToolResultTool::new(store);

    for id in ["old", "new"] {
        let c = call(id, "shell");
        trunc
            .after_tool(&c, result(&c, json!({"stdout": "z".repeat(1500)})))
            .await
            .unwrap();
    }
    assert!(
        reader.call(json!({"callId": "old"}), &ctx()).await.is_err(),
        "oldest entry evicted"
    );
    assert!(reader.call(json!({"callId": "new"}), &ctx()).await.is_ok());
}
