//! The tool inspector: renders a single `ToolActivity`'s input and output
//! as wrapped, syntax-highlighted, size-truncated lines for the split-view
//! inspector pane. Pure rendering — reads no `App` state.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::tui::components::inspector::CodePreview;
use crate::tui::components::section::Section;
use crate::view::{self, theme};

mod language;
mod preview;
#[cfg(test)]
pub(super) use preview::shallow_json_preview;
mod summary;
use language::{inspector_output_language, language_for_path};
pub(super) use preview::{
    inspector_output_preview, inspector_text_content, limit_inspector_preview,
};

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
    let action_width = width.saturating_sub(2 + view::cell_width(&mode_suffix));
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
        "shell" => "Run a shell command",
        "exec_command" => "Execute a command",
        "read_file" => "Read a file",
        "write_file" => "Write a file",
        "edit_file" => "Edit a file",
        "multi_edit" => "Apply ordered exact edits",
        "apply_patch" => "Apply a multi-file patch",
        "list_dir" => "List a directory",
        "grep" => "Search workspace text",
        "web_search" => "Search the web",
        "glob" => "Find workspace paths",
        "process" => "Manage a background process",
        "pykernel" => "Run Python in the persistent kernel",
        "bun_repl" => "Run JavaScript or TypeScript in the persistent Bun REPL",
        "subagent" => "Delegate a focused task",
        "todo_write" => "Update the task list",
        "ask" => "Ask for clarification",
        "web_fetch" => "Fetch a web document",
        "web_crawl" => "Crawl web pages",
        "read_tool_result" => "Read a stored tool result",
        "skill" => "Load skill instructions",
        "mcp_search_tools" => "Search connected capabilities",
        "mcp_select_tool" => "Select a connected capability",
        "mcp_features" => "Use an MCP server feature",
        "copy_file" => "Copy a file",
        "rename_file" => "Rename a file",
        "delete_file" => "Delete a file",
        "create_folder" => "Create a folder",
        "file_info" => "Inspect file metadata",
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
    let mut input = Section::inspector("input", t.dim);
    append_inspector_input(&mut input, tool, inner);
    input.append_to(&mut lines);
    let mut output_section = Section::inspector("output", t.dim);
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
        .saturating_sub(view::cell_width(label) + view::cell_width(action))
        .max(2);
    Line::from(vec![
        Span::styled(format!("  {label}"), theme().dim),
        Span::raw(" ".repeat(gap)),
        Span::styled(action.to_string(), theme().accent),
    ])
}

fn append_inspector_input(section: &mut Section, tool: &ToolActivity, width: usize) {
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

fn push_inspector_text(section: &mut Section, text: &str, width: usize, style: Style) {
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
