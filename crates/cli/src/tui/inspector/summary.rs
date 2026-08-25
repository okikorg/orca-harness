use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::tui::components::inspector::{
    inspector_fields, inspector_text, CodePreview, InspectorSection, INSPECTOR_BODY_INDENT,
};
use crate::tui::components::progress_list::{progress_list, ProgressItem, ProgressState};
use crate::view::{self, theme};

use super::{inspector_size_label, language_for_path, limit_inspector_preview, ToolActivity};

pub(super) fn lines(tool: &ToolActivity, width: usize) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(2).max(16);
    let mut lines = Vec::new();
    append_input(&mut lines, tool, inner);
    append_result(&mut lines, tool, inner);
    lines
}

fn section(label: impl Into<String>) -> InspectorSection {
    InspectorSection::new(label, theme().dim)
}

fn append_input(lines: &mut Vec<Line<'static>>, tool: &ToolActivity, width: usize) {
    match tool.tool_name.as_str() {
        "shell" | "exec_command" => text_section(
            lines,
            "command",
            string(&tool.input, "command"),
            width,
            theme().code,
        ),
        "pykernel" | "bun_repl" => append_code_input(lines, tool, width),
        "edit_file" => append_edit_input(lines, tool, width),
        "write_file" => append_file_content(lines, tool, width),
        "read_file" | "list_dir" => text_section(
            lines,
            "path",
            string(&tool.input, "path"),
            width,
            Style::default(),
        ),
        "grep" => {
            let mut input = section("search");
            input.extend(inspector_fields(
                [
                    ("query", string(&tool.input, "query").to_owned()),
                    ("path", string(&tool.input, "path").to_owned()),
                ]
                .into_iter()
                .filter(|(_, value)| !value.is_empty()),
                width,
            ));
            input.append_to(lines);
        }
        "glob" => {
            let mut input = section("search");
            input.extend(inspector_fields(
                [
                    ("pattern", string(&tool.input, "pattern").to_owned()),
                    ("path", string(&tool.input, "path").to_owned()),
                ]
                .into_iter()
                .filter(|(_, value)| !value.is_empty()),
                width,
            ));
            input.append_to(lines);
        }
        "process" => append_process_input(lines, tool, width),
        "todo_write" if tool.output.is_none() => append_todos(lines, &tool.input, width),
        "todo_write" => {}
        "ask" => append_ask(lines, &tool.input, "request", width),
        "subagent" => text_section(
            lines,
            "task",
            string(&tool.input, "task"),
            width,
            Style::default(),
        ),
        "web_fetch" | "web_crawl" => text_section(
            lines,
            "url",
            string(&tool.input, "url"),
            width,
            theme().code,
        ),
        "web_search" | "mcp_search_tools" => text_section(
            lines,
            "query",
            string(&tool.input, "query"),
            width,
            Style::default(),
        ),
        "skill" => {
            let mut input = section("skill");
            input.extend(inspector_fields(
                [
                    ("name", string(&tool.input, "name").to_owned()),
                    ("resource", string(&tool.input, "resource").to_owned()),
                ]
                .into_iter()
                .filter(|(_, value)| !value.is_empty()),
                width,
            ));
            input.append_to(lines);
        }
        _ => append_generic_value(lines, "request", &tool.input, width),
    }
}

fn append_result(lines: &mut Vec<Line<'static>>, tool: &ToolActivity, width: usize) {
    let Some(output) = &tool.output else {
        text_section(
            lines,
            "result",
            waiting_label(&tool.tool_name),
            width,
            theme().dim,
        );
        return;
    };
    if tool.is_error {
        let message = output
            .get("error")
            .and_then(Value::as_str)
            .or_else(|| output.as_str())
            .unwrap_or("Tool execution failed");
        text_section(lines, "error", message, width, theme().error);
        return;
    }

    match tool.tool_name.as_str() {
        "shell" | "exec_command" => append_shell_result(lines, output, width),
        "read_file" => append_file_result(lines, tool, output, width),
        "write_file" | "edit_file" => append_mutation_result(lines, output, width),
        "list_dir" => append_collection(lines, output, "entries", "entries", width),
        "grep" => append_grep_result(lines, output, width),
        "glob" => append_collection(lines, output, "matches", "matches", width),
        "process" => append_process_result(lines, output, width),
        "todo_write" => append_todos(lines, output, width),
        "ask" => append_ask(lines, output, "answers", width),
        "pykernel" | "bun_repl" => append_kernel_result(lines, output, width),
        "subagent" => append_subagent_result(lines, output, width),
        "web_fetch" => append_web_fetch_result(lines, output, width),
        "web_search" | "mcp_search_tools" => append_ranked_results(lines, output, width),
        "web_crawl" => append_crawl_result(lines, output, width),
        "read_tool_result" => append_paged_result(lines, output, width),
        "skill" => append_skill_result(lines, output, width),
        _ => append_generic_value(lines, "result", output, width),
    }
    append_partial_notice(lines, output, width);
}

fn append_code_input(lines: &mut Vec<Line<'static>>, tool: &ToolActivity, width: usize) {
    let action = string(&tool.input, "action");
    if action == "reset" {
        text_section(
            lines,
            "action",
            "Reset persistent session",
            width,
            Style::default(),
        );
        return;
    }
    let code = string(&tool.input, "code");
    let (code, omitted) = limit_inspector_preview(code);
    let language = if tool.tool_name == "pykernel" {
        "python"
    } else {
        "typescript"
    };
    let mut input = section(format!("code · {language}"));
    input.extend(
        CodePreview {
            text: &code,
            language,
            width,
            indent: INSPECTOR_BODY_INDENT,
            plain_style: Style::default(),
        }
        .lines(),
    );
    if omitted {
        input.push(Line::from(Span::styled(
            format!("{INSPECTOR_BODY_INDENT}code preview shortened"),
            theme().dim,
        )));
    }
    input.append_to(lines);
}

fn append_file_content(lines: &mut Vec<Line<'static>>, tool: &ToolActivity, width: usize) {
    let path = string(&tool.input, "path");
    let content = string(&tool.input, "content");
    let language = language_for_path(path).unwrap_or("text");
    let mut input = section(format!(
        "content · {language} · {} · {}",
        line_label(content),
        inspector_size_label(content.len() as u64)
    ));
    let (preview, omitted) = limit_inspector_preview(content);
    input.extend(
        CodePreview {
            text: &preview,
            language,
            width,
            indent: INSPECTOR_BODY_INDENT,
            plain_style: Style::default(),
        }
        .lines(),
    );
    if omitted {
        input.push(Line::from(Span::styled(
            format!("{INSPECTOR_BODY_INDENT}input preview shortened"),
            theme().dim,
        )));
    }
    input.append_to(lines);
}

fn append_edit_input(lines: &mut Vec<Line<'static>>, tool: &ToolActivity, width: usize) {
    text_section(
        lines,
        "path",
        string(&tool.input, "path"),
        width,
        Style::default(),
    );
    let mut change = section("change");
    for (prefix, field, style) in [("- ", "old", theme().error), ("+ ", "new", theme().success)] {
        let text = view::sanitize_cells(string(&tool.input, field));
        let (text, omitted) = limit_inspector_preview(&text);
        for line in text.lines() {
            change.push(Line::from(Span::styled(
                view::truncate_line(&format!("    {prefix}{line}"), width),
                style,
            )));
        }
        if omitted {
            change.push(Line::from(Span::styled(
                "    change preview shortened",
                theme().dim,
            )));
        }
    }
    change.append_to(lines);
}

fn append_process_input(lines: &mut Vec<Line<'static>>, tool: &ToolActivity, width: usize) {
    let mut input = section("process");
    input.extend(inspector_fields(
        [
            ("action", string(&tool.input, "action").to_owned()),
            ("id", string(&tool.input, "id").to_owned()),
            ("command", string(&tool.input, "command").to_owned()),
            ("input", string(&tool.input, "input").to_owned()),
        ]
        .into_iter()
        .filter(|(_, value)| !value.is_empty()),
        width,
    ));
    input.append_to(lines);
}

fn append_shell_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    for (field, label, style) in [
        ("stdout", "stdout", Style::default()),
        ("stderr", "stderr", theme().error),
    ] {
        let text = string(output, field);
        if text.trim().is_empty() {
            continue;
        }
        let (preview, omitted) = window_text(text);
        let mut section = section(format!("{label} · {}", line_label(text)));
        section.extend(inspector_text(&preview, width, style));
        if omitted {
            section.push(Line::from(Span::styled(
                "    output preview shortened",
                theme().dim,
            )));
        }
        section.append_to(lines);
    }
    let exit = output
        .get("exitCode")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let mut result = section(format!("result · exit {exit}"));
    if string(output, "stdout").is_empty() && string(output, "stderr").is_empty() {
        result.extend(inspector_text(
            "Command finished without output",
            width,
            theme().dim,
        ));
    }
    result.append_to(lines);
}

fn append_file_result(
    lines: &mut Vec<Line<'static>>,
    tool: &ToolActivity,
    output: &Value,
    width: usize,
) {
    let content = string(output, "content");
    let path = string(&tool.input, "path");
    let language = language_for_path(path).unwrap_or("text");
    let bytes = output
        .get("bytes")
        .and_then(Value::as_u64)
        .unwrap_or(content.len() as u64);
    let mut source = section(format!(
        "source · {language} · {} · {}",
        line_label(content),
        inspector_size_label(bytes)
    ));
    if content.is_empty() {
        source.extend(inspector_text("This file is empty", width, theme().dim));
    } else {
        let (preview, omitted) = limit_inspector_preview(content);
        source.extend(
            CodePreview {
                text: &preview,
                language,
                width,
                indent: INSPECTOR_BODY_INDENT,
                plain_style: Style::default(),
            }
            .lines(),
        );
        if omitted {
            source.push(Line::from(Span::styled(
                format!("{INSPECTOR_BODY_INDENT}source preview shortened"),
                theme().dim,
            )));
        }
    }
    source.append_to(lines);
}

fn append_mutation_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let mut rows = Vec::new();
    if let Some(path) = output.get("path").and_then(Value::as_str) {
        rows.push(("path", path.to_owned()));
    }
    if let Some(bytes) = output.get("bytesWritten").and_then(Value::as_u64) {
        rows.push(("written", inspector_size_label(bytes)));
    }
    if let Some(count) = output.get("replacements").and_then(Value::as_u64) {
        rows.push(("replacements", count.to_string()));
    }
    let mut result = section("result");
    result.extend(inspector_fields(rows, width));
    result.append_to(lines);
}

fn append_collection(
    lines: &mut Vec<Line<'static>>,
    output: &Value,
    field: &str,
    label: &str,
    width: usize,
) {
    let Some(items) = output.get(field).and_then(Value::as_array) else {
        append_generic_value(lines, "result", output, width);
        return;
    };
    let mut result = section(format!("{label} · {}", items.len()));
    if items.is_empty() {
        result.extend(inspector_text(&format!("No {label}"), width, theme().dim));
    } else {
        for item in items.iter().take(40) {
            let text = collection_item(item);
            result.push(Line::from(Span::raw(view::truncate_line(
                &format!("    {text}"),
                width,
            ))));
        }
        if items.len() > 40 {
            result.push(Line::from(Span::styled(
                format!("    {} more", items.len() - 40),
                theme().dim,
            )));
        }
    }
    result.append_to(lines);
}

fn append_grep_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let Some(matches) = output.get("matches").and_then(Value::as_array) else {
        append_generic_value(lines, "result", output, width);
        return;
    };
    let mut result = section(format!("matches · {}", matches.len()));
    if matches.is_empty() {
        result.extend(inspector_text("No matching lines", width, theme().dim));
    } else {
        for item in matches.iter().take(40) {
            let text = match item {
                Value::Object(map) => {
                    let path = map.get("path").and_then(Value::as_str).unwrap_or("");
                    let line = map
                        .get("line")
                        .and_then(Value::as_u64)
                        .map(|n| n.to_string())
                        .unwrap_or_default();
                    let body = map.get("text").and_then(Value::as_str).unwrap_or("");
                    format!("{path}:{line}  {body}")
                }
                _ => compact_value(item),
            };
            result.push(Line::from(Span::raw(view::truncate_line(
                &format!("    {text}"),
                width,
            ))));
        }
    }
    result.append_to(lines);
}

fn append_process_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    if output.get("processes").is_some() {
        append_collection(lines, output, "processes", "processes", width);
        return;
    }
    let state = if output.get("running").and_then(Value::as_bool) == Some(true) {
        "running".to_owned()
    } else if let Some(exit) = output.get("exitCode").and_then(Value::as_i64) {
        format!("exit {exit}")
    } else {
        "stopped".to_owned()
    };
    let mut process = section("process");
    process.extend(inspector_fields(
        [("id", string(output, "id").to_owned()), ("state", state)]
            .into_iter()
            .filter(|(_, value)| !value.is_empty()),
        width,
    ));
    process.append_to(lines);
    let detail = string(output, "output");
    if !detail.is_empty() {
        let (preview, omitted) = window_text(detail);
        text_section(lines, "output", &preview, width, Style::default());
        if omitted {
            text_section(
                lines,
                "partial result",
                "Output preview shortened",
                width,
                theme().dim,
            );
        }
    }
}

fn append_ask(lines: &mut Vec<Line<'static>>, value: &Value, label: &str, width: usize) {
    let Some(topics) = value.get("topics").and_then(Value::as_array) else {
        append_generic_value(lines, label, value, width);
        return;
    };
    let mut section = section(format!("{label} · {} topics", topics.len()));
    for topic in topics.iter().take(3) {
        let title = ["topic", "title", "question"]
            .into_iter()
            .find_map(|field| topic.get(field).and_then(Value::as_str))
            .unwrap_or("Topic");
        section.push(Line::from(Span::styled(
            view::truncate_line(&format!("    {title}"), width),
            theme().strong,
        )));
        if let Some(questions) = topic.get("questions").and_then(Value::as_array) {
            for question in questions.iter().take(4) {
                let text = string(question, "question");
                if !text.is_empty() {
                    section.push(Line::from(Span::raw(view::truncate_line(
                        &format!("      {text}"),
                        width,
                    ))));
                }
            }
        }
        if let Some(answers) = topic.get("answers").and_then(Value::as_array) {
            for answer in answers.iter().take(4) {
                let values = answer
                    .get("values")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                if !values.is_empty() {
                    section.push(Line::from(Span::raw(view::truncate_line(
                        &format!("      ✓ {values}"),
                        width,
                    ))));
                }
            }
        }
        let context = topic
            .get("additionalContext")
            .or_else(|| topic.get("additional_context"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !context.is_empty() {
            section.push(Line::from(Span::raw(view::truncate_line(
                &format!("      {context}"),
                width,
            ))));
        }
    }
    section.append_to(lines);
}

fn append_todos(lines: &mut Vec<Line<'static>>, value: &Value, width: usize) {
    let Some(items) = value.get("todos").and_then(Value::as_array) else {
        return;
    };
    if items.is_empty() {
        text_section(lines, "todo", "No active tasks", width, theme().dim);
        return;
    }
    let owned: Vec<(String, ProgressState)> = items
        .iter()
        .take(20)
        .map(|item| {
            let content = string(item, "content").to_owned();
            let state = match string(item, "status") {
                "completed" => ProgressState::Completed,
                "in_progress" => ProgressState::Active,
                _ => ProgressState::Pending,
            };
            (content, state)
        })
        .collect();
    let refs: Vec<_> = owned
        .iter()
        .map(|(content, state)| ProgressItem {
            content,
            state: *state,
        })
        .collect();
    lines.extend(progress_list("todo", &refs, width));
    if items.len() > owned.len() {
        lines.push(Line::from(Span::styled(
            format!("    {} more tasks", items.len() - owned.len()),
            theme().dim,
        )));
    }
}

fn append_kernel_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let state = string(output, "state");
    let mut result = section(format!(
        "result · {}",
        if state.is_empty() { "complete" } else { state }
    ));
    let text = string(output, "output");
    if text.is_empty() {
        result.extend(inspector_text(
            "Execution completed without printed output",
            width,
            theme().dim,
        ));
    } else {
        let (preview, omitted) = limit_inspector_preview(text);
        result.extend(inspector_text(&preview, width, Style::default()));
        if omitted {
            result.push(Line::from(Span::styled(
                "    output preview shortened",
                theme().dim,
            )));
        }
    }
    result.append_to(lines);
    for (field, label, style) in [
        ("stderr", "stderr", theme().error),
        ("traceback", "traceback", theme().error),
        ("error", "error", theme().warn),
    ] {
        let text = string(output, field);
        if !text.is_empty() {
            text_section(lines, label, text, width, style);
        }
    }
}

fn append_subagent_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    text_section(
        lines,
        "answer",
        string(output, "answer"),
        width,
        Style::default(),
    );
    if let Some(usage) = output.get("usage") {
        append_generic_value(lines, "usage", usage, width);
    }
}

fn append_web_fetch_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let mut response = section("response");
    response.extend(inspector_fields(
        [
            (
                "status",
                output.get("status").map(compact_value).unwrap_or_default(),
            ),
            ("type", string(output, "contentType").to_owned()),
            ("final url", string(output, "finalUrl").to_owned()),
        ]
        .into_iter()
        .filter(|(_, value)| !value.is_empty()),
        width,
    ));
    response.append_to(lines);
    let content = string(output, "content");
    if !content.is_empty() {
        text_section(lines, "content", content, width, Style::default());
    }
}

fn append_ranked_results(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let field = if output.get("results").is_some() {
        "results"
    } else {
        "tools"
    };
    append_collection(lines, output, field, field, width);
}

fn append_crawl_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let mut result = section("crawl");
    result.extend(inspector_fields(
        [
            ("status", string(output, "status").to_owned()),
            (
                "pages",
                output
                    .get("pagesReturned")
                    .map(compact_value)
                    .unwrap_or_default(),
            ),
            (
                "total",
                output
                    .get("totalPages")
                    .map(compact_value)
                    .unwrap_or_default(),
            ),
        ]
        .into_iter()
        .filter(|(_, value)| !value.is_empty()),
        width,
    ));
    result.append_to(lines);
    if let Some(pages) = output.get("pages").and_then(Value::as_array) {
        let mut list = section(format!("pages · {}", pages.len()));
        for page in pages.iter().take(30) {
            let title = string(page, "title");
            let url = string(page, "url");
            let text = if title.is_empty() { url } else { title };
            list.push(Line::from(Span::raw(view::truncate_line(
                &format!("    {text}"),
                width,
            ))));
        }
        list.append_to(lines);
    }
}

fn append_paged_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let mut page = section("stored result");
    page.extend(inspector_fields(
        [
            ("tool", string(output, "toolName").to_owned()),
            (
                "offset",
                output.get("offset").map(compact_value).unwrap_or_default(),
            ),
            (
                "total",
                output
                    .get("totalChars")
                    .map(compact_value)
                    .unwrap_or_default(),
            ),
        ]
        .into_iter()
        .filter(|(_, value)| !value.is_empty()),
        width,
    ));
    let content = string(output, "content");
    if !content.is_empty() {
        page.extend(inspector_text(content, width, Style::default()));
    }
    page.append_to(lines);
}

fn append_skill_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let instructions = string(output, "instructions");
    if !instructions.is_empty() {
        text_section(lines, "instructions", instructions, width, Style::default());
    }
    if let Some(resources) = output.get("resources") {
        append_generic_value(lines, "resources", resources, width);
    }
}

fn append_generic_value(lines: &mut Vec<Line<'static>>, label: &str, value: &Value, width: usize) {
    if let Some(text) = value.as_str() {
        text_section(lines, label, text, width, Style::default());
        return;
    }
    if let Some(map) = value.as_object() {
        let document = ["content", "text", "message", "answer", "markdown"]
            .into_iter()
            .find_map(|key| map.get(key).and_then(Value::as_str).map(|text| (key, text)));
        let mut rows = Vec::new();
        for (key, child) in map {
            if document.is_some_and(|(document_key, _)| document_key == key) {
                continue;
            }
            rows.push((key.as_str(), compact_value(child)));
        }
        if !rows.is_empty() {
            let mut fields = section(label);
            fields.extend(inspector_fields(rows, width));
            fields.append_to(lines);
        }
        if let Some((key, text)) = document {
            text_section(lines, key, text, width, Style::default());
        }
        return;
    }
    if let Some(values) = value.as_array() {
        let mut result = section(format!("{label} · {} items", values.len()));
        for value in values.iter().take(40) {
            result.push(Line::from(Span::raw(view::truncate_line(
                &format!("    {}", compact_value(value)),
                width,
            ))));
        }
        result.append_to(lines);
        return;
    }
    text_section(lines, label, &compact_value(value), width, Style::default());
}

fn append_partial_notice(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let notice = if output
        .get("droppedBytes")
        .and_then(Value::as_u64)
        .is_some_and(|n| n > 0)
    {
        Some("Older output was dropped")
    } else if output.get("moreOutput").and_then(Value::as_bool) == Some(true) {
        Some("More output is buffered")
    } else if output.get("hasMore").and_then(Value::as_bool) == Some(true) {
        Some("More content is available")
    } else if [
        "truncated",
        "stdoutTruncated",
        "stderrTruncated",
        "_truncated",
    ]
    .into_iter()
    .any(|field| output.get(field).and_then(Value::as_bool) == Some(true))
    {
        Some("The result is partial")
    } else if output.get("timedOut").and_then(Value::as_bool) == Some(true) {
        Some("The operation reached its time limit")
    } else {
        None
    };
    if let Some(notice) = notice {
        text_section(lines, "partial result", notice, width, theme().warn);
    }
}

fn text_section(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    text: &str,
    width: usize,
    style: Style,
) {
    if text.is_empty() {
        return;
    }
    let (preview, omitted) = limit_inspector_preview(text);
    let mut section = section(label);
    section.extend(inspector_text(&preview, width, style));
    if omitted {
        section.push(Line::from(Span::styled(
            "    preview shortened",
            theme().dim,
        )));
    }
    section.append_to(lines);
}

fn collection_item(value: &Value) -> String {
    let Some(map) = value.as_object() else {
        return compact_value(value);
    };
    let fields = [
        "name",
        "title",
        "id",
        "state",
        "running",
        "command",
        "server",
        "url",
        "description",
        "snippet",
        "text",
    ];
    let parts: Vec<String> = fields
        .into_iter()
        .filter_map(|field| {
            let value = map.get(field)?;
            (!value.is_object() && !value.is_array())
                .then(|| compact_value(value))
                .filter(|value| !value.is_empty())
        })
        .take(3)
        .collect();
    if parts.is_empty() {
        compact_value(value)
    } else {
        parts.join(" · ")
    }
}

fn compact_value(value: &Value) -> String {
    match value {
        Value::Object(map) => format!("{} fields", map.len()),
        Value::Array(values) => format!("{} items", values.len()),
        Value::String(text) => text.to_owned(),
        Value::Null => "none".to_owned(),
        _ => value.to_string(),
    }
}

fn string<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or("")
}

fn line_label(text: &str) -> String {
    match text.lines().count() {
        0 => "empty".to_owned(),
        1 => "1 line".to_owned(),
        count => format!("{count} lines"),
    }
}

fn window_text(text: &str) -> (String, bool) {
    let lines: Vec<_> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.len() <= 22 {
        return (lines.join("\n"), false);
    }
    let mut shown = lines[..16].to_vec();
    shown.extend_from_slice(&lines[lines.len() - 6..]);
    (shown.join("\n"), true)
}

fn waiting_label(tool_name: &str) -> &'static str {
    match tool_name {
        "ask" => "Waiting for your answer",
        "subagent" => "The delegated agent is working",
        "web_crawl" => "Collecting pages",
        "process" => "Waiting for process state",
        _ => "Waiting for result",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect()
    }

    #[test]
    fn object_collection_rows_keep_distinguishing_fields() {
        let item = serde_json::json!({
            "title": "Building terminal UIs",
            "url": "https://ratatui.rs",
            "snippet": "Layout patterns"
        });
        assert_eq!(
            collection_item(&item),
            "Building terminal UIs · https://ratatui.rs · Layout patterns"
        );
    }

    #[test]
    fn ask_summary_keeps_questions_and_answers() {
        let value = serde_json::json!({
            "topics": [{
                "topic": "Scope",
                "questions": [{"question": "Which tools?"}],
                "answers": [{"values": ["All tools"]}]
            }]
        });
        let mut lines = Vec::new();
        append_ask(&mut lines, &value, "answers", 80);
        let rendered = text(&lines);
        assert!(rendered.contains("Scope"));
        assert!(rendered.contains("Which tools?"));
        assert!(rendered.contains("✓ All tools"));
    }
}
