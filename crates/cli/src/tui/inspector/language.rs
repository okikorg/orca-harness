//! Tool output language detection, isolated from inspector layout.

use std::path::Path;

use super::super::ToolActivity;

pub(super) fn inspector_output_language(
    tool: &ToolActivity,
    output: &serde_json::Value,
) -> Option<&'static str> {
    let path = tool
        .input
        .get("path")
        .or_else(|| tool.input.get("file_path"))
        .and_then(serde_json::Value::as_str);
    if let Some(language) = path.and_then(language_for_path) {
        return Some(language);
    }
    if (output.is_object() || output.is_array())
        && !matches!(
            tool.tool_name.as_str(),
            "shell" | "process" | "grep" | "read_file" | "list_dir"
        )
    {
        Some("json")
    } else {
        None
    }
}

pub(super) fn language_for_path(path: &str) -> Option<&'static str> {
    let extension = Path::new(path).extension()?.to_str()?;
    match extension.to_ascii_lowercase().as_str() {
        "rs" => Some("rust"),
        "js" | "jsx" => Some("javascript"),
        "ts" | "tsx" => Some("typescript"),
        "py" => Some("python"),
        "go" => Some("go"),
        "sh" | "bash" | "zsh" => Some("bash"),
        "json" => Some("json"),
        "toml" => Some("toml"),
        "yaml" | "yml" => Some("yaml"),
        "md" => Some("markdown"),
        "html" => Some("html"),
        "css" => Some("css"),
        "sql" => Some("sql"),
        _ => None,
    }
}
