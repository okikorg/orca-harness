//! Provider-neutral summaries for tool protocol calls and results.

use serde_json::Value;

use crate::view::truncate_line;

/// Short, human-readable verbs used by the TUI for tool activity rows.
///
/// Keep this exhaustive for built-in tools: falling back to the protocol
/// name makes snake_case identifiers leak into the user-facing activity rail.
pub fn tool_action_label(name: &str) -> &'static str {
    match name {
        "shell" => "Shell",
        "exec_command" => "Exec command",
        "read_file" => "Read",
        "write_file" => "Write",
        "edit_file" => "Edit",
        "multi_edit" => "Multi-edit",
        "apply_patch" => "Apply patch",
        "list_dir" => "List directory",
        "grep" => "Search text",
        "web_search" => "Web search",
        "glob" => "Find",
        "process" => "Manage process",
        "pykernel" => "PyKernel",
        "bun_repl" => "Bun REPL",
        "subagent" => "Subagent",
        "workflow" => "Workflow",
        "todo_write" => "Todo",
        "ask" => "Ask",
        "web_fetch" => "Fetch web page",
        "web_crawl" => "Crawl web pages",
        "read_tool_result" => "Read tool result",
        "skill" => "Load skill",
        "mcp_search_tools" => "Search MCP tools",
        "mcp_select_tool" => "Select MCP tool",
        "mcp_features" => "Use MCP feature",
        "copy_file" => "Copy file",
        "rename_file" => "Rename file",
        "delete_file" => "Delete file",
        "create_folder" => "Create folder",
        "file_info" => "Inspect file info",
        _ => "",
    }
}

pub fn tool_call_line(name: &str, args: &Value) -> String {
    let detail = match name {
        "shell" => args
            .get("command")
            .and_then(Value::as_str)
            .map(|c| format!("$ {c}")),
        "read_file" | "write_file" | "edit_file" | "list_dir" => {
            args.get("path").and_then(Value::as_str).map(str::to_string)
        }
        "apply_patch" => patch_call_detail(args),
        "multi_edit" => multi_edit_call_detail(args),
        "grep" => args.get("query").and_then(Value::as_str).map(|query| {
            let path = args
                .get("path")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty() && *p != ".");
            match path {
                Some(path) => format!("'{query}' in {path}"),
                None => format!("'{query}'"),
            }
        }),
        "glob" => glob_call_detail(args),
        "skill" => args.get("name").and_then(Value::as_str).map(|name| {
            args.get("resource")
                .and_then(Value::as_str)
                .filter(|resource| !resource.is_empty())
                .map(|resource| format!("{name} · {resource}"))
                .unwrap_or_else(|| name.to_string())
        }),
        "subagent" => args.get("task").and_then(Value::as_str).map(str::to_string),
        "workflow" => Some(
            match args.get("action").and_then(Value::as_str).unwrap_or("run") {
                "run" => format!(
                    "{} stages",
                    args.get("graph")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len)
                ),
                action => format!(
                    "{action}{}",
                    args.get("runId")
                        .map(|id| format!(" #{id}"))
                        .unwrap_or_default()
                ),
            },
        ),
        "web_fetch" | "web_crawl" => args.get("url").and_then(Value::as_str).map(str::to_string),
        "web_search" => args
            .get("query")
            .and_then(Value::as_str)
            .map(|query| format!("'{query}'")),
        "process" => process_call_detail(args),
        _ => None,
    };
    let detail = detail.unwrap_or_else(|| first_text_field(args));
    truncate_line(format!("{name} {detail}").trim_end(), 100)
}

/// A readable stand-in for an unknown shape: the first string field, the
/// key list for an object of non-strings, and nothing for an empty value.
/// Serialized JSON is never shown; it is the model's, not the reader's.
fn first_text_field(value: &Value) -> String {
    match value {
        Value::Object(map) => map
            .iter()
            .find_map(|(_, v)| v.as_str().filter(|s| !s.trim().is_empty()))
            .map(str::to_string)
            .unwrap_or_else(|| map.keys().cloned().collect::<Vec<_>>().join(" ")),
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn glob_call_detail(args: &Value) -> Option<String> {
    let pattern = args.get("pattern").and_then(Value::as_str)?;
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty());
    Some(match path {
        Some(path) => format!("{path}/{pattern}"),
        None => pattern.to_string(),
    })
}

fn multi_edit_call_detail(args: &Value) -> Option<String> {
    let edits = args.get("edits")?.as_array()?;
    let files = edits
        .iter()
        .filter_map(|edit| edit.get("path").and_then(Value::as_str))
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    Some(format!(
        "{} · {}",
        count_label(edits.len(), "edit", "edits"),
        count_label(files, "file", "files")
    ))
}

fn patch_call_detail(args: &Value) -> Option<String> {
    let patch = args.get("patch")?.as_str()?;
    let paths: Vec<&str> = patch
        .lines()
        .filter_map(|line| {
            line.strip_prefix("*** Add File: ")
                .or_else(|| line.strip_prefix("*** Update File: "))
                .or_else(|| line.strip_prefix("*** Delete File: "))
        })
        .collect();
    match paths.as_slice() {
        [] => Some("no file operations".into()),
        [path] => Some((*path).to_owned()),
        [first, ..] => Some(format!(
            "{} · {}",
            first,
            count_label(paths.len(), "file", "files")
        )),
    }
}

pub(super) fn process_call_detail(args: &Value) -> Option<String> {
    let action = args.get("action")?.as_str()?;
    let id = args.get("id").and_then(Value::as_str);
    Some(match action {
        "spawn" => {
            let command = args.get("command").and_then(Value::as_str).unwrap_or("?");
            format!("spawn $ {command}")
        }
        "poll" => {
            let wait = args
                .get("waitMs")
                .and_then(Value::as_u64)
                .map(|ms| {
                    if ms >= 1_000 && ms % 1_000 == 0 {
                        format!(" · wait {}s", ms / 1_000)
                    } else {
                        format!(" · wait {ms}ms")
                    }
                })
                .unwrap_or_default();
            format!("poll {}{wait}", id.unwrap_or("?"))
        }
        "write" => {
            let input = args.get("input").and_then(Value::as_str).unwrap_or("");
            format!("write {} “{}”", id.unwrap_or("?"), truncate_line(input, 40))
        }
        "kill" => format!("kill {}", id.unwrap_or("?")),
        "list" => "list".to_string(),
        other => other.to_string(),
    })
}

/// The compact outcome line for a finished tool call.
pub fn tool_result_summary(name: &str, output: &Value, is_error: bool) -> String {
    if is_error {
        if name == "shell" {
            if let Some(exit) = output.get("exitCode").and_then(Value::as_i64) {
                return format!("exit {exit}");
            }
        }
        let message = output
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| output.as_str().map(str::to_string))
            .unwrap_or_else(|| output.to_string());
        return truncate_line(&format!("error: {message}"), 120);
    }
    let summary = match name {
        "shell" => {
            let exit = output
                .get("exitCode")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let stdout = output.get("stdout").and_then(Value::as_str).unwrap_or("");
            let stderr = output.get("stderr").and_then(Value::as_str).unwrap_or("");
            let head = [stdout, stderr]
                .iter()
                .flat_map(|s| s.lines())
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            Some(if head.is_empty() {
                format!("exit {exit}")
            } else {
                format!("exit {exit} · {head}")
            })
        }
        "read_file" => output
            .get("bytes")
            .and_then(Value::as_u64)
            .map(|b| format!("read {}", byte_label(b))),
        "write_file" => output
            .get("bytesWritten")
            .and_then(Value::as_u64)
            .map(|b| format!("wrote {}", byte_label(b))),
        "edit_file" => output
            .get("replacements")
            .and_then(Value::as_u64)
            .map(|n| count_label(n as usize, "replacement", "replacements")),
        "multi_edit" => output
            .get("filesChanged")
            .and_then(Value::as_u64)
            .map(|files| {
                let edits = output
                    .get("editsApplied")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                format!("{edits} edits · {files} files")
            }),
        "apply_patch" => output
            .get("filesChanged")
            .and_then(Value::as_u64)
            .map(|files| {
                let added = output
                    .get("added")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                let updated = output
                    .get("updated")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                let deleted = output
                    .get("deleted")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                format!("{files} files · +{added} ~{updated} -{deleted}")
            }),
        "list_dir" => output
            .get("entries")
            .and_then(Value::as_array)
            .map(|e| format!("{} entries", e.len())),
        "glob" | "grep" => matches_summary(output),
        "skill" => output.get("name").and_then(Value::as_str).map(|name| {
            match output.get("alreadyLoaded") == Some(&Value::Bool(true)) {
                true => format!("already loaded {name}"),
                false => format!("loaded {name}"),
            }
        }),
        "subagent" => subagent_result_summary(output),
        "web_fetch" => web_fetch_result_summary(output),
        "web_search" => output
            .get("results")
            .and_then(Value::as_array)
            .map(|results| count_label(results.len(), "result", "results")),
        "web_crawl" => web_crawl_result_summary(output),
        "process" => process_result_summary(output),
        _ => None,
    };
    let summary = summary.unwrap_or_else(|| {
        let text = first_text_field(output);
        if text.is_empty() {
            "ok".to_string()
        } else {
            text
        }
    });
    truncate_line(&summary, 120)
}

/// `3 bytes`, `5.0 kB`, `1.2 MB`.
pub fn byte_label(bytes: u64) -> String {
    match bytes {
        0..=1023 => count_label(bytes as usize, "byte", "bytes"),
        1024..=1_048_575 => format!("{:.1} kB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

fn matches_summary(output: &Value) -> Option<String> {
    let matches = output.get("matches").and_then(Value::as_array)?;
    let suffix = if output.get("truncated").and_then(Value::as_bool) == Some(true) {
        "+"
    } else {
        ""
    };
    Some(format!(
        "{count}{suffix} {label}",
        count = matches.len(),
        label = if matches.len() == 1 {
            "match"
        } else {
            "matches"
        }
    ))
}

fn subagent_result_summary(output: &Value) -> Option<String> {
    let termination = output
        .get("termination")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    let mut parts = vec![termination.to_string()];
    if let Some(steps) = output.get("steps").and_then(Value::as_u64) {
        parts.push(count_label(steps as usize, "step", "steps"));
    }
    if let Some(tools) = output.get("toolCalls").and_then(Value::as_u64) {
        parts.push(count_label(tools as usize, "tool", "tools"));
    }
    Some(parts.join(" · "))
}

fn web_fetch_result_summary(output: &Value) -> Option<String> {
    let status = output.get("status").and_then(Value::as_u64)?;
    let mut summary = format!("status {status}");
    if let Some(content_type) = output
        .get("contentType")
        .and_then(Value::as_str)
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        summary.push_str(" · ");
        summary.push_str(content_type);
    }
    if output.get("truncated").and_then(Value::as_bool) == Some(true) {
        summary.push_str(" · truncated");
    }
    Some(summary)
}

fn web_crawl_result_summary(output: &Value) -> Option<String> {
    let pages = output.get("pagesReturned").and_then(Value::as_u64)? as usize;
    let mut summary = count_label(pages, "page", "pages");
    if output.get("timedOut").and_then(Value::as_bool) == Some(true) {
        summary.push_str(" · timed out");
    } else if let Some(status) = output.get("status").and_then(Value::as_str) {
        summary.push_str(" · ");
        summary.push_str(status);
    }
    Some(summary)
}

fn count_label(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

pub(super) fn process_result_summary(output: &Value) -> Option<String> {
    if let Some(processes) = output.get("processes").and_then(Value::as_array) {
        return Some(match processes.len() {
            0 => "no processes".to_string(),
            1 => "1 process".to_string(),
            count => format!("{count} processes"),
        });
    }

    let id = output.get("id").and_then(Value::as_str)?;
    let state = if output.get("running").and_then(Value::as_bool) == Some(true) {
        "running".to_string()
    } else if let Some(code) = output.get("exitCode").and_then(Value::as_i64) {
        format!("exit {code}")
    } else {
        "stopped".to_string()
    };
    let output_head = output
        .get("output")
        .and_then(Value::as_str)
        .and_then(|text| text.lines().find(|line| !line.trim().is_empty()))
        .map(str::trim);

    Some(match output_head {
        Some(head) => format!("{id} · {state} · {head}"),
        None => format!("{id} · {state}"),
    })
}
