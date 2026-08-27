use super::*;

#[test]
fn tool_list_prefixes_names_and_requires_input_schemas() {
    let listed = json!({ "tools": [
        { "name": "echo", "description": "echo back",
          "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } } },
        { "name": "bare", "inputSchema": { "type": "object" } },
    ]});
    let page = parse_tool_page("docs", &listed).unwrap();
    let tools = page.tools;
    assert_eq!(tools[0].0.name, "mcp__docs__echo");
    assert_eq!(tools[0].0.description, "echo back");
    assert_eq!(tools[0].1, "echo");
    assert_eq!(tools[1].0.name, "mcp__docs__bare");
    assert_eq!(tools[1].0.description, "");
    assert_eq!(tools[1].0.parameters, json!({ "type": "object" }));
}

#[test]
fn tool_list_rejects_malformed_results() {
    assert!(parse_tool_page("d", &json!({})).is_err());
    assert!(parse_tool_page("d", &json!({ "tools": [{ "description": "nameless" }] })).is_err());
    assert!(parse_tool_page("d", &json!({ "tools": [{ "name": "bare" }] })).is_err());
}

#[test]
fn initialize_requires_the_supported_version_and_valid_capabilities() {
    let valid = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {}, "resources": {} },
        "serverInfo": { "name": "fake", "version": "1" }
    });
    let capabilities = parse_initialize(&valid).unwrap();
    assert!(capabilities.tools);
    assert!(capabilities.resources);
    assert!(!capabilities.prompts);
    let older = json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "serverInfo": { "name": "fake", "version": "1" }
    });
    assert!(parse_initialize(&older).is_ok());
    assert!(parse_initialize(&json!({})).is_err());
    let mut unsupported = valid;
    unsupported["protocolVersion"] = json!("old");
    assert!(parse_initialize(&unsupported).is_err());
}

#[test]
fn tool_page_parses_and_validates_next_cursor() {
    let page = parse_tool_page("docs", &json!({ "tools": [], "nextCursor": "two" })).unwrap();
    assert_eq!(page.next_cursor.as_deref(), Some("two"));
    assert!(parse_tool_page("docs", &json!({ "tools": [], "nextCursor": 2 })).is_err());
    assert!(parse_tool_page("docs", &json!({ "tools": [], "nextCursor": "" })).is_err());
}

#[test]
fn duplicate_tool_names_are_rejected() {
    let tools = vec![
        (
            ToolSchema {
                name: "mcp__same".into(),
                description: String::new(),
                parameters: json!({}),
            },
            "one".into(),
        ),
        (
            ToolSchema {
                name: "mcp__same".into(),
                description: String::new(),
                parameters: json!({}),
            },
            "two".into(),
        ),
    ];
    assert!(ensure_unique_tools(&tools).is_err());
}

#[test]
fn call_output_joins_text_and_prefers_structured_content() {
    let text = json!({ "content": [
        { "type": "text", "text": "one" },
        { "type": "text", "text": "two" },
    ]});
    assert_eq!(
        call_output(&text).unwrap(),
        json!({ "content": "one\ntwo" })
    );

    let structured = json!({ "content": [], "structuredContent": { "count": 3 } });
    assert_eq!(call_output(&structured).unwrap(), json!({ "count": 3 }));

    // Non-text content passes through raw rather than being dropped.
    let image = json!({ "content": [{ "type": "image", "data": "abc" }] });
    assert_eq!(
        call_output(&image).unwrap(),
        json!({ "content": [{ "type": "image", "data": "abc" }] })
    );
}

#[test]
fn call_output_rejects_malformed_results() {
    for malformed in [
        json!(null),
        json!({}),
        json!({ "content": null }),
        json!({ "content": [], "isError": "yes" }),
    ] {
        assert!(call_output(&malformed).is_err(), "accepted {malformed}");
    }
}

#[test]
fn call_output_surfaces_is_error_as_a_tool_error() {
    let failed = json!({ "isError": true, "content": [{ "type": "text", "text": "boom" }] });
    assert_eq!(call_output(&failed).unwrap_err().message, "boom");
    let silent = json!({ "isError": true, "content": [] });
    assert_eq!(
        call_output(&silent).unwrap_err().message,
        "tool reported an error"
    );
}
