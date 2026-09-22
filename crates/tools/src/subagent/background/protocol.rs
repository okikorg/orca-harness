use super::{BackgroundConfig, SubagentManager};
use orca_harness_core::ToolError;
use serde_json::{json, Value};

pub(in crate::subagent) const BACKGROUND_DELIVERY: &str = "Background results are delivered automatically between your steps or wake you after your turn ends. Continue independent work or answer new user requests while agents run. When only background work remains, end your turn with a brief status so the user can continue; you will be notified when results arrive. Do not wait, sleep, or poll for completion. Stopping or interrupting the parent does not stop background agents. Cancel them only when the user explicitly asks to stop them.";

pub(in crate::subagent) fn subagent_parameters(
    manager: Option<&SubagentManager>,
    mut properties: serde_json::Map<String, Value>,
    requires_model: bool,
) -> Value {
    let run_required = if requires_model {
        json!(["task", "model"])
    } else {
        json!(["task"])
    };
    let Some(_) = manager else {
        return json!({
            "type": "object",
            "properties": properties,
            "required": run_required,
        });
    };
    properties.insert(
        "background".into(),
        json!({
            "type": "boolean",
            "default": true,
            "description": "Defaults to true: return immediately and deliver this agent's result to the parent later. Set false explicitly for a foreground call that waits for the answer."
        }),
    );
    properties.insert(
        "persistent".into(),
        json!({"type": "boolean", "default": false, "description": "Opt in to a session-scoped sidekick whose conversation is retained for later action=task calls. Persistent tasks run in the background by default; set background=false to await one explicitly. Ordinary one-shot behavior remains the default."}),
    );
    properties.insert(
        "action".into(),
        json!({
            "type": "string",
            "enum": ["run", "task", "status", "stop", "list", "cancel", "cancel_all"],
            "description": format!("Run a task (default), send a task to an idle sidekick, inspect or stop a sidekick handle, list active ordinary background agents, cancel one ordinary spawnId, or cancel all ordinary background agents. {BACKGROUND_DELIVERY}")
        }),
    );
    properties.insert(
        "spawnId".into(),
        json!({
            "type": "integer",
            "minimum": 0,
            "description": "Spawn to address for task, status, stop, or cancel."
        }),
    );
    json!({
        "type": "object",
        "properties": properties,
        "oneOf": [
            {"properties": {"action": {"enum": ["run"]}}, "required": run_required},
            {"properties": {"action": {"const": "task"}}, "required": ["action", "spawnId", "task"]},
            {"properties": {"action": {"const": "status"}}, "required": ["action", "spawnId"]},
            {"properties": {"action": {"const": "stop"}}, "required": ["action", "spawnId"]},
            {"properties": {"action": {"const": "list"}}, "required": ["action"]},
            {"properties": {"action": {"const": "cancel"}}, "required": ["action", "spawnId"]},
            {"properties": {"action": {"const": "cancel_all"}}, "required": ["action"]}
        ]
    })
}

pub(in crate::subagent) fn subagent_control(
    background: Option<&BackgroundConfig>,
    input: &Value,
) -> Option<Result<Value, ToolError>> {
    let action = input.get("action").and_then(Value::as_str).unwrap_or("run");
    if action == "run" {
        return None;
    }
    let Some(background) = background else {
        return Some(Err(ToolError::msg(
            "background subagent controls are unavailable in this host or nesting depth",
        )));
    };
    let manager: &SubagentManager = &background.manager;
    Some(match action {
        "list" => {
            let agents = manager.active();
            let snapshot = agents
                .iter()
                .map(|job| (job.spawn.id, job.status))
                .collect::<Vec<_>>();
            let mut last = background.last_list.lock().unwrap();
            if !snapshot.is_empty() && last.as_ref() == Some(&snapshot) {
                return Some(Err(ToolError::msg(format!(
                    "no change since the previous list. {BACKGROUND_DELIVERY}"
                ))));
            }
            *last = Some(snapshot);
            Ok(json!({
                "count": agents.len(),
                "agents": agents.into_iter().map(|job| json!({
                    "spawnId": job.spawn.id,
                    "parentId": job.spawn.parent_id,
                    "depth": job.spawn.depth,
                    "task": job.spawn.task,
                    "identity": job.spawn.identity,
                    "status": job.status.as_str(),
                })).collect::<Vec<_>>(),
            }))
        }
        "cancel" => {
            let Some(spawn_id) = input.get("spawnId").and_then(Value::as_u64) else {
                return Some(Err(ToolError::msg(
                    "`spawnId` (non-negative integer) is required for action=cancel",
                )));
            };
            Ok(json!({
                "spawnId": spawn_id,
                "cancelled": manager.cancel(spawn_id),
            }))
        }
        "cancel_all" => Ok(json!({
            "cancelled": manager.cancel_all(),
        })),
        // Old conversation history can still contain wait calls. Reject them
        // immediately so they cannot occupy the parent while children run.
        "wait" => Err(ToolError::msg(format!(
            "action=wait is no longer supported. {BACKGROUND_DELIVERY}"
        ))),
        other => Err(ToolError::msg(format!(
            "unknown subagent action `{other}`; expected run, task, status, stop, list, cancel, or cancel_all"
        ))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_action_lists_sidekick_controls() {
        let manager = SubagentManager::default();
        let background = BackgroundConfig {
            manager,
            notifier: std::sync::Arc::new(|_| {}),
            last_list: Default::default(),
        };

        let error = subagent_control(Some(&background), &json!({"action": "bogus"}))
            .expect("control action")
            .expect_err("unknown action")
            .to_string();

        assert!(error.contains("run, task, status, stop, list, cancel, or cancel_all"));
    }
}
