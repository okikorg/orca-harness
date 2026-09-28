//! Fail-closed extraction of plan-area targets from mutating tool calls.

use orca_harness_core::{ToolCall, ToolResult};

fn plan_path(path: Option<&str>) -> Option<String> {
    path.filter(|path| crate::plan::is_plan_path(path))
        .map(str::to_owned)
}

/// Every path a write-shaped call targets, only when the complete call stays
/// inside the plan area. An `edit_file` batch fails closed on one bad entry,
/// and `edits` wins over a top-level `path` exactly as the tool reads it.
pub(super) fn written_paths(call: &ToolCall) -> Option<Vec<String>> {
    let path = |value: &serde_json::Value| plan_path(value.get("path")?.as_str());
    match call.name.as_str() {
        "edit_file" if call.arguments.get("edits").is_some() => {
            let edits = call.arguments.get("edits")?.as_array()?;
            if edits.is_empty() {
                return None;
            }
            edits.iter().map(path).collect()
        }
        "write_file" | "edit_file" => Some(vec![path(&call.arguments)?]),
        _ => None,
    }
}

pub(super) fn result_paths(result: &ToolResult) -> Vec<String> {
    let output = &result.output;
    match result.tool_name.as_str() {
        "write_file" => output.get("path").into_iter().collect(),
        "edit_file" => output
            .get("paths")
            .and_then(|paths| paths.as_array())
            .into_iter()
            .flatten()
            .collect(),
        _ => Vec::new(),
    }
    .into_iter()
    .filter_map(|path| plan_path(path.as_str()))
    .collect()
}
