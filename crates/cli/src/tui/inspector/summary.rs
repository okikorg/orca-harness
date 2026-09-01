use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::tui::components::inspector::{
    inspector_fields, inspector_text, CodePreview, INSPECTOR_BODY_INDENT,
};
use crate::tui::components::section::Section;
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

fn section(label: impl Into<String>) -> Section {
    Section::inspector(label, theme().dim)
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
        "multi_edit" => append_multi_edit_input(lines, tool, width),
        "apply_patch" => append_patch_input(lines, tool, width),
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
        "write_file" | "edit_file" | "multi_edit" | "apply_patch" => {
            append_mutation_result(lines, output, width)
        }
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

fn append_multi_edit_input(lines: &mut Vec<Line<'static>>, tool: &ToolActivity, width: usize) {
    let Some(edits) = tool.input.get("edits") else {
        return;
    };
    let count = edits.as_array().map(Vec::len).unwrap_or_default();
    append_generic_value(lines, &format!("edits · {count}"), edits, width);
}

fn append_patch_input(lines: &mut Vec<Line<'static>>, tool: &ToolActivity, width: usize) {
    let patch = string(&tool.input, "patch");
    let (preview, omitted) = limit_inspector_preview(patch);
    let mut input = section(format!(
        "patch · {} · {}",
        line_label(patch),
        inspector_size_label(patch.len() as u64)
    ));
    input.extend(
        CodePreview {
            text: &preview,
            language: "diff",
            width,
            indent: INSPECTOR_BODY_INDENT,
            plain_style: Style::default(),
        }
        .lines(),
    );
    if omitted {
        input.push(Line::from(Span::styled(
            format!("{INSPECTOR_BODY_INDENT}patch preview shortened"),
            theme().dim,
        )));
    }
    input.append_to(lines);
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

include!("summary/results.rs");
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
