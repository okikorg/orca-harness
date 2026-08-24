#[test]
fn inspector_caps_large_file_previews() {
    let output = serde_json::Value::String(
        (0..400)
            .map(|line| format!("line {line}: {}", "x".repeat(200)))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let (preview, omitted) = inspector_output_preview("read_file", &output, None, false);
    assert!(omitted);
    assert!(preview.len() <= INSPECTOR_PREVIEW_CHARS);
    assert!(preview.lines().count() <= INSPECTOR_PREVIEW_LINES);
}

#[test]
fn inspector_shell_output_leads_with_signal_and_keeps_the_tail() {
    let stdout = (0..40)
        .map(|line| format!("build line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let output = serde_json::json!({
        "stdout": stdout,
        "stderr": "",
        "exitCode": 0,
        "success": true
    });
    let (preview, omitted) = inspector_output_preview("shell", &output, None, false);
    assert!(preview.starts_with("exit 0 · 40 stdout\n"));
    assert!(preview.contains("build line 0"));
    assert!(preview.contains("build line 39"));
    assert!(!preview.contains("build line 20"));
    assert!(omitted);
}

#[test]
fn inspector_collection_output_shows_count_without_json_scaffolding() {
    let output = serde_json::json!({
        "query": "ToolActivity",
        "matches": ["src/tui.rs:181", "src/tui.rs:1861"],
        "truncated": false
    });
    let (preview, omitted) = inspector_output_preview("grep", &output, Some("json"), false);
    assert_eq!(preview, "2 matches\nsrc/tui.rs:181\nsrc/tui.rs:1861");
    assert!(!omitted);
}

#[test]
fn inspector_classifies_results_by_shape_not_tool_name() {
    let execution = serde_json::json!({
        "stdout": "compiled",
        "stderr": "",
        "exitCode": 0
    });
    let (preview, _) = inspector_output_preview("custom_runner", &execution, None, false);
    assert_eq!(preview, "exit 0 · 1 stdout\ncompiled");

    let mutation = serde_json::json!({"path": "src/lib.rs", "bytesWritten": 2048});
    let (preview, _) = inspector_output_preview("custom_writer", &mutation, None, false);
    assert_eq!(preview, "src/lib.rs · wrote 2.0 KiB");
}

#[test]
fn inspector_json_source_renders_content_instead_of_its_envelope() {
    let output = serde_json::json!({
        "content": "{\n  \"name\": \"orca\"\n}",
        "bytes": 20,
        "truncated": false
    });
    let (preview, omitted) = inspector_output_preview("anything", &output, Some("json"), false);
    assert_eq!(preview, "{\n  \"name\": \"orca\"\n}");
    assert!(!omitted);
}

#[test]
fn inspector_source_output_reports_language_shape_and_size() {
    let tool = ToolActivity {
        call_id: "read-1".into(),
        call_line: "read_file src/main.rs".into(),
        tool_name: "read_file".into(),
        input: serde_json::json!({"path": "src/main.rs"}),
        started: Instant::now(),
        elapsed: Some(Duration::from_millis(1)),
        output: None,
        is_error: false,
        approval: None,
    };
    let output = serde_json::json!({
        "content": "fn main() {\n    println!(\"orca\");\n}\n",
        "bytes": 2048,
        "truncated": false
    });
    assert_eq!(
        inspector_code_facts(&tool, &output, Some("rust")).as_deref(),
        Some("rust · 3 lines · 2.0 KiB")
    );
}
