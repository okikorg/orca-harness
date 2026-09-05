//! Bounded output previews for the tool inspector.

use super::{
    inspector_size_label, view, INSPECTOR_OUTPUT_HEAD, INSPECTOR_OUTPUT_TAIL,
    INSPECTOR_PREVIEW_CHARS, INSPECTOR_PREVIEW_LINES,
};

pub(in crate::tui) fn shallow_json_preview(output: &serde_json::Value) -> (String, bool) {
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

pub(in crate::tui) fn inspector_output_preview(
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

pub(in crate::tui) fn inspector_text_content(output: &serde_json::Value) -> String {
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

pub(in crate::tui) fn limit_inspector_preview(expanded: &str) -> (String, bool) {
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
