//! Provider-neutral summaries for tool protocol calls and results.

use serde_json::Value;

use crate::view::truncate_line;

pub fn tool_call_line(name: &str, args: &Value) -> String {
    let detail = match name {
        "shell" => args
            .get("command")
            .and_then(Value::as_str)
            .map(|c| format!("$ {c}")),
        "read_file" | "write_file" | "edit_file" | "list_dir" => {
            args.get("path").and_then(Value::as_str).map(str::to_string)
        }
        "grep" => args
            .get("query")
            .and_then(Value::as_str)
            .map(|q| format!("'{q}'")),
        "process" => process_call_detail(args),
        _ => None,
    };
    let detail = detail.unwrap_or_else(|| args.to_string());
    truncate_line(&format!("{name} {detail}"), 100)
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
            .map(|b| format!("read {b} bytes")),
        "write_file" => output
            .get("bytesWritten")
            .and_then(Value::as_u64)
            .map(|b| format!("wrote {b} bytes")),
        "edit_file" => output
            .get("replacements")
            .and_then(Value::as_u64)
            .map(|n| format!("{n} replacement(s)")),
        "list_dir" => output
            .get("entries")
            .and_then(Value::as_array)
            .map(|e| format!("{} entries", e.len())),
        "process" => process_result_summary(output),
        _ => None,
    };
    let summary = summary.unwrap_or_else(|| output.to_string());
    truncate_line(&summary, 120)
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
