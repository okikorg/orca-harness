//! The tool inspector: renders a single `ToolActivity`'s input and output
//! as wrapped, syntax-highlighted, size-truncated lines for the split-view
//! inspector pane. Pure rendering — reads no `App` state.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::tui::components::inspector::{CodePreview, InspectorSection};
use crate::view::{self, theme};

mod language;
mod summary;
use language::{inspector_output_language, language_for_path};

use super::format::tool_timing_label;
use super::{
    InspectorMode, ToolActivity, INSPECTOR_OUTPUT_HEAD, INSPECTOR_OUTPUT_TAIL,
    INSPECTOR_PREVIEW_CHARS, INSPECTOR_PREVIEW_LINES,
};

#[cfg(test)]
pub(super) fn tool_inspector_lines(tool: &ToolActivity, width: usize) -> Vec<Line<'static>> {
    let mut lines = tool_inspector_header_lines(tool, width, InspectorMode::Summary);
    lines.extend(tool_inspector_body_lines(
        tool,
        width,
        InspectorMode::Summary,
    ));
    lines
}

pub(super) fn tool_inspector_header_lines(
    tool: &ToolActivity,
    width: usize,
    mode: InspectorMode,
) -> Vec<Line<'static>> {
    let t = theme();
    let (status, status_style) = inspector_status(tool);
    let raw_name = tool.tool_name.to_lowercase();
    let duration = tool_timing_label(tool, true);
    let identity = format!("{raw_name} · {status} · {duration}");
    let mode_suffix = format!(" · {}", mode.label().to_lowercase());
    let action_width = width.saturating_sub(2 + mode_suffix.chars().count());
    let action = view::truncate_line(inspector_action_label(&tool.tool_name), action_width);
    vec![
        Line::from(Span::styled(
            format!(
                "  {}",
                view::truncate_line(&identity, width.saturating_sub(4))
            ),
            status_style,
        )),
        Line::from(Span::styled(format!("  {action}{mode_suffix}"), t.dim)),
    ]
}

fn inspector_status(tool: &ToolActivity) -> (&'static str, Style) {
    let t = theme();
    let Some(output) = &tool.output else {
        return if tool.elapsed.is_some() {
            if tool.is_error {
                ("failed", t.error)
            } else {
                ("completed", t.success)
            }
        } else {
            ("running", t.accent)
        };
    };
    if tool.is_error {
        return ("failed", t.error);
    }
    let warning = output.get("success").and_then(serde_json::Value::as_bool) == Some(false)
        || output
            .get("exitCode")
            .and_then(serde_json::Value::as_i64)
            .is_some_and(|code| code != 0)
        || matches!(
            output.get("state").and_then(serde_json::Value::as_str),
            Some("error" | "timeout")
        )
        || output
            .get("status")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|status| status >= 400)
        || output.get("timedOut").and_then(serde_json::Value::as_bool) == Some(true);
    if warning {
        ("completed with warnings", t.warn)
    } else {
        ("completed", t.success)
    }
}

fn inspector_action_label(tool_name: &str) -> &'static str {
    match tool_name {
        "shell" | "exec_command" => "Run a shell command",
        "read_file" => "Read a file",
        "write_file" => "Write a file",
        "edit_file" => "Edit a file",
        "multi_edit" => "Apply ordered exact edits",
        "apply_patch" => "Apply a multi-file patch",
        "list_dir" => "List a directory",
        "grep" => "Search workspace text",
        "glob" => "Find workspace paths",
        "process" => "Manage a background process",
        "pykernel" => "Run Python in the persistent kernel",
        "bun_repl" => "Run JavaScript or TypeScript in the persistent Bun REPL",
        "subagent" => "Delegate a focused task",
        "todo_write" => "Update the task list",
        "ask" => "Ask for clarification",
        "web_fetch" => "Fetch a web document",
        "web_search" => "Search the web",
        "web_crawl" => "Crawl web pages",
        "read_tool_result" => "Read a stored tool result",
        "skill" => "Load skill instructions",
        "mcp_search_tools" => "Search connected capabilities",
        "mcp_select_tool" => "Select a connected capability",
        "mcp_features" => "Use an MCP server feature",
        _ => "Inspect tool input and output",
    }
}

pub(super) fn tool_inspector_body_lines(
    tool: &ToolActivity,
    width: usize,
    mode: InspectorMode,
) -> Vec<Line<'static>> {
    if mode == InspectorMode::Summary {
        return summary::lines(tool, width);
    }
    debug_inspector_body_lines(tool, width)
}

fn debug_inspector_body_lines(tool: &ToolActivity, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let inner = width.saturating_sub(4).max(16);
    let mut lines = Vec::new();
    let mut input = InspectorSection::new("input", t.dim);
    append_inspector_input(&mut input, tool, inner);
    input.append_to(&mut lines);
    let mut output_section = InspectorSection::new("output", t.dim);
    if let Some(output) = &tool.output {
        let language = inspector_output_language(tool, output);
        if let Some(facts) = inspector_code_facts(tool, output, language) {
            output_section.push(Line::from(Span::styled(format!("  {facts}"), t.dim)));
        }
        let (expanded, omitted) =
            inspector_output_preview(&tool.tool_name, output, language, tool.is_error);
        if tool.is_error {
            push_inspector_text(&mut output_section, &expanded, inner, t.error);
        } else if let Some(language) = language {
            output_section.extend(
                CodePreview {
                    text: &expanded,
                    language,
                    width: inner,
                    indent: "  ",
                    plain_style: Style::default(),
                }
                .lines(),
            );
        } else {
            push_inspector_text(&mut output_section, &expanded, inner, Style::default());
        }
        if omitted {
            output_section.push(Line::from(""));
            output_section.push(inspector_omitted_line(inner, "output omitted", "/expand n"));
        }
    } else {
        output_section.push(Line::from(Span::styled("  waiting for result", t.dim)));
    }
    output_section.append_to(&mut lines);
    lines
}

fn inspector_omitted_line(width: usize, label: &str, action: &str) -> Line<'static> {
    let gap = width
        .saturating_sub(label.chars().count() + action.chars().count())
        .max(2);
    Line::from(vec![
        Span::styled(format!("  {label}"), theme().dim),
        Span::raw(" ".repeat(gap)),
        Span::styled(action.to_string(), theme().accent),
    ])
}

fn append_inspector_input(section: &mut InspectorSection, tool: &ToolActivity, width: usize) {
    let path = tool.input.get("path").and_then(serde_json::Value::as_str);
    let content = tool
        .input
        .get("content")
        .and_then(serde_json::Value::as_str);
    if let (Some(path), Some(content)) = (path, content) {
        let language = language_for_path(path).unwrap_or("text");
        let line_count = content.lines().count();
        let line_label = if line_count == 1 { "line" } else { "lines" };
        section.push(Line::from(Span::styled(
            format!(
                "  {path} · {language} · {line_count} {line_label} · {}",
                inspector_size_label(content.len() as u64)
            ),
            theme().dim,
        )));
        let (preview, omitted) = limit_inspector_preview(content);
        if language == "text" {
            push_inspector_text(section, &preview, width, Style::default());
        } else {
            section.extend(
                CodePreview {
                    text: &preview,
                    language,
                    width,
                    indent: "  ",
                    plain_style: Style::default(),
                }
                .lines(),
            );
        }
        if omitted {
            section.push(Line::from(Span::styled(
                "  More input omitted",
                theme().dim,
            )));
        }
        return;
    }

    let input =
        serde_json::to_string_pretty(&tool.input).unwrap_or_else(|_| tool.input.to_string());
    section.extend(
        CodePreview {
            text: &input,
            language: "json",
            width,
            indent: "  ",
            plain_style: Style::default(),
        }
        .lines(),
    );
}

pub(super) fn inspector_code_facts(
    tool: &ToolActivity,
    output: &serde_json::Value,
    language: Option<&str>,
) -> Option<String> {
    if tool.tool_name != "read_file" || tool.is_error {
        return None;
    }
    let content = output.get("content")?.as_str()?;
    let language = language?;
    let lines = content.lines().count();
    let bytes = output
        .get("bytes")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(content.len() as u64);
    let truncated = output
        .get("truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let range = match lines {
        0 => "empty".to_owned(),
        1 => "1 line".to_owned(),
        count => format!("{count} lines"),
    };
    let suffix = if truncated { " · truncated" } else { "" };
    Some(format!(
        "{language} · {range} · {}{suffix}",
        inspector_size_label(bytes)
    ))
}

pub(super) fn inspector_size_label(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

pub(super) fn shallow_json_preview(output: &serde_json::Value) -> (String, bool) {
    fn child(value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::Object(_) => "{ … }".to_owned(),
            serde_json::Value::Array(_) => "[ … ]".to_owned(),
            _ => serde_json::to_string(value).unwrap_or_else(|_| value.to_string()),
        }
    }

    let max_children = INSPECTOR_PREVIEW_LINES.saturating_sub(2);
    let (text, omitted) = match output {
        serde_json::Value::Object(map) => {
            let shown = map.len().min(max_children);
            let mut lines = Vec::with_capacity(shown + 2);
            lines.push("{".to_owned());
            for (index, (key, value)) in map.iter().take(shown).enumerate() {
                let comma = if index + 1 < shown { "," } else { "" };
                let key = serde_json::to_string(key).unwrap_or_else(|_| format!("\"{key}\""));
                lines.push(format!("  {key}: {}{comma}", child(value)));
            }
            lines.push("}".to_owned());
            (lines.join("\n"), map.len() > shown)
        }
        serde_json::Value::Array(values) => {
            let shown = values.len().min(max_children);
            let mut lines = Vec::with_capacity(shown + 2);
            lines.push("[".to_owned());
            for (index, value) in values.iter().take(shown).enumerate() {
                let comma = if index + 1 < shown { "," } else { "" };
                lines.push(format!("  {}{comma}", child(value)));
            }
            lines.push("]".to_owned());
            (lines.join("\n"), values.len() > shown)
        }
        _ => (
            serde_json::to_string_pretty(output).unwrap_or_else(|_| output.to_string()),
            false,
        ),
    };
    let (text, size_omitted) = limit_inspector_preview(&text);
    (text, omitted || size_omitted)
}

pub(super) fn inspector_output_preview(
    tool_name: &str,
    output: &serde_json::Value,
    language: Option<&str>,
    is_error: bool,
) -> (String, bool) {
    if is_error {
        return (view::tool_result_summary(tool_name, output, true), false);
    }
    match classify_inspector_output(output, language) {
        InspectorOutputKind::Execution => shell_inspector_preview(output),
        InspectorOutputKind::Collection { field, label } => {
            collection_inspector_preview(output, field, label)
        }
        InspectorOutputKind::Process => process_inspector_preview(output),
        InspectorOutputKind::Mutation => mutation_inspector_preview(output),
        InspectorOutputKind::Structured => shallow_json_preview(output),
        InspectorOutputKind::Text => {
            let expanded = inspector_text_content(output);
            limit_inspector_preview(&expanded)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InspectorOutputKind {
    Execution,
    Collection {
        field: &'static str,
        label: &'static str,
    },
    Process,
    Mutation,
    Structured,
    Text,
}

fn classify_inspector_output(
    output: &serde_json::Value,
    language: Option<&str>,
) -> InspectorOutputKind {
    if output.get("stdout").is_some() || output.get("stderr").is_some() {
        InspectorOutputKind::Execution
    } else if output
        .get("entries")
        .is_some_and(serde_json::Value::is_array)
    {
        InspectorOutputKind::Collection {
            field: "entries",
            label: "entries",
        }
    } else if output
        .get("matches")
        .is_some_and(serde_json::Value::is_array)
    {
        InspectorOutputKind::Collection {
            field: "matches",
            label: "matches",
        }
    } else if output
        .get("processes")
        .is_some_and(serde_json::Value::is_array)
        || (output.get("id").is_some()
            && (output.get("running").is_some() || output.get("output").is_some()))
    {
        InspectorOutputKind::Process
    } else if output.get("bytesWritten").is_some() || output.get("replacements").is_some() {
        InspectorOutputKind::Mutation
    } else if output.is_string()
        || output
            .get("content")
            .is_some_and(serde_json::Value::is_string)
    {
        InspectorOutputKind::Text
    } else if language == Some("json") || output.is_object() || output.is_array() {
        InspectorOutputKind::Structured
    } else {
        InspectorOutputKind::Text
    }
}

pub(super) fn inspector_text_content(output: &serde_json::Value) -> String {
    output
        .as_str()
        .or_else(|| output.get("content").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .unwrap_or_else(|| output.to_string())
}

fn mutation_inspector_preview(output: &serde_json::Value) -> (String, bool) {
    let path = output
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(|path| format!("{path} · "))
        .unwrap_or_default();
    if let Some(bytes) = output
        .get("bytesWritten")
        .and_then(serde_json::Value::as_u64)
    {
        return (
            format!("{path}wrote {}", inspector_size_label(bytes)),
            false,
        );
    }
    if let Some(replacements) = output
        .get("replacements")
        .and_then(serde_json::Value::as_u64)
    {
        let label = if replacements == 1 {
            "replacement"
        } else {
            "replacements"
        };
        return (format!("{path}{replacements} {label}"), false);
    }
    shallow_json_preview(output)
}

fn shell_inspector_preview(output: &serde_json::Value) -> (String, bool) {
    let exit = output
        .get("exitCode")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_default();
    let stdout = output
        .get("stdout")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let stderr = output
        .get("stderr")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let stdout_count = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    let stderr_count = stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    let mut text = format!("exit {exit}");
    if stdout_count > 0 {
        text.push_str(&format!(" · {stdout_count} stdout"));
    }
    if stderr_count > 0 {
        text.push_str(&format!(" · {stderr_count} stderr"));
    }
    let mut detail: Vec<&str> = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|line| !line.trim().is_empty())
        .collect();
    detail.dedup_by(|a, b| a.trim() == b.trim());
    let (shown, omitted) = informative_line_window(&detail);
    if !shown.is_empty() {
        text.push('\n');
        text.push_str(&shown.join("\n"));
    }
    let (text, size_omitted) = limit_inspector_preview(&text);
    (text, omitted || size_omitted)
}

fn collection_inspector_preview(
    output: &serde_json::Value,
    field: &str,
    label: &str,
) -> (String, bool) {
    let Some(items) = output.get(field).and_then(serde_json::Value::as_array) else {
        return shallow_json_preview(output);
    };
    let values: Vec<String> = items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| item.to_string())
        })
        .collect();
    let refs: Vec<&str> = values.iter().map(String::as_str).collect();
    let (shown, window_omitted) = informative_line_window(&refs);
    let truncated = output
        .get("truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let mut text = format!("{} {label}", items.len());
    if !shown.is_empty() {
        text.push('\n');
        text.push_str(&shown.join("\n"));
    }
    (text, truncated || window_omitted)
}

fn process_inspector_preview(output: &serde_json::Value) -> (String, bool) {
    let mut text = view::tool_result_summary("process", output, false);
    let detail = output
        .get("output")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let lines: Vec<&str> = detail
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .collect();
    let (shown, omitted) = informative_line_window(&lines);
    if !shown.is_empty() {
        text.push('\n');
        text.push_str(&shown.join("\n"));
    }
    let (text, size_omitted) = limit_inspector_preview(&text);
    (text, omitted || size_omitted)
}

fn informative_line_window<'a>(lines: &[&'a str]) -> (Vec<&'a str>, bool) {
    let limit = INSPECTOR_OUTPUT_HEAD + INSPECTOR_OUTPUT_TAIL;
    if lines.len() <= limit {
        return (lines.to_vec(), false);
    }
    let mut shown = lines[..INSPECTOR_OUTPUT_HEAD].to_vec();
    shown.extend_from_slice(&lines[lines.len() - INSPECTOR_OUTPUT_TAIL..]);
    (shown, true)
}

pub(super) fn limit_inspector_preview(expanded: &str) -> (String, bool) {
    let mut preview = String::new();
    let mut omitted = expanded.lines().count() > INSPECTOR_PREVIEW_LINES;
    for (index, line) in expanded.lines().take(INSPECTOR_PREVIEW_LINES).enumerate() {
        let separator = usize::from(index > 0);
        let remaining = INSPECTOR_PREVIEW_CHARS.saturating_sub(preview.len() + separator);
        if remaining == 0 {
            omitted = true;
            break;
        }
        if index > 0 {
            preview.push('\n');
        }
        if line.len() > remaining {
            let end = line
                .char_indices()
                .map(|(offset, _)| offset)
                .take_while(|offset| *offset <= remaining)
                .last()
                .unwrap_or(0);
            preview.push_str(&line[..end]);
            omitted = true;
            break;
        }
        preview.push_str(line);
    }
    (preview, omitted)
}

pub(super) fn empty_tool_inspector_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled("  Tool Inspector", theme().strong)),
        Line::from(""),
        Line::from(Span::styled(
            "  Tool input and output will appear here.",
            theme().dim,
        )),
    ]
}

fn push_inspector_text(section: &mut InspectorSection, text: &str, width: usize, style: Style) {
    let text = view::sanitize_cells(text);
    for source in text.lines() {
        let wrapped = textwrap::wrap(source, width.saturating_sub(2).max(8));
        if wrapped.is_empty() {
            section.push(Line::from(""));
        } else {
            for part in wrapped {
                section.push(Line::from(Span::styled(format!("  {part}"), style)));
            }
        }
    }
}
