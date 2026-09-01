//! Fail-closed extraction of plan-area targets from mutating tool calls.

use orca_harness_core::{ToolCall, ToolResult};

fn plan_path(path: Option<&str>) -> Option<String> {
    path.filter(|path| crate::plan::is_plan_path(path))
        .map(str::to_owned)
}

/// Every path a write-shaped call targets, only when the complete call stays
/// inside the plan area. Batch tools fail closed on one bad entry.
pub(super) fn written_paths(call: &ToolCall) -> Option<Vec<String>> {
    match call.name.as_str() {
        "write_file" | "edit_file" => Some(vec![plan_path(
            call.arguments.get("path").and_then(|path| path.as_str()),
        )?]),
        "multi_edit" => {
            let edits = call.arguments.get("edits")?.as_array()?;
            if edits.is_empty() {
                return None;
            }
            edits
                .iter()
                .map(|edit| plan_path(edit.get("path").and_then(|path| path.as_str())))
                .collect()
        }
        "apply_patch" => patch_paths(
            call.arguments
                .get("patch")
                .and_then(|patch| patch.as_str())?,
        ),
        _ => None,
    }
}

fn patch_paths(patch: &str) -> Option<Vec<String>> {
    let lines: Vec<&str> = patch
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if lines.first() != Some(&"*** Begin Patch") || lines.last() != Some(&"*** End Patch") {
        return None;
    }
    let mut paths = Vec::new();
    for line in lines {
        if line.starts_with("*** Delete File: ") {
            // Plan mode may create or revise plans, not remove them.
            return None;
        }
        if let Some(path) = line
            .strip_prefix("*** Add File: ")
            .or_else(|| line.strip_prefix("*** Update File: "))
        {
            paths.push(plan_path(Some(path))?);
        }
    }
    (!paths.is_empty()).then_some(paths)
}

pub(super) fn result_paths(result: &ToolResult) -> Vec<String> {
    match result.tool_name.as_str() {
        "write_file" | "edit_file" => result
            .output
            .get("path")
            .and_then(|path| path.as_str())
            .and_then(|path| plan_path(Some(path)))
            .into_iter()
            .collect(),
        "multi_edit" | "apply_patch" => result
            .output
            .get("paths")
            .and_then(|paths| paths.as_array())
            .into_iter()
            .flatten()
            .filter_map(|path| path.as_str())
            .filter_map(|path| plan_path(Some(path)))
            .collect(),
        _ => Vec::new(),
    }
}
