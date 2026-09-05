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
        "action".into(),
        json!({
            "type": "string",
            "enum": ["run", "list", "cancel", "cancel_all"],
            "description": format!("Run a task (default), list active background agents, cancel one spawnId, or cancel all active background agents. {BACKGROUND_DELIVERY}")
        }),
    );
    properties.insert(
        "spawnId".into(),
        json!({
            "type": "integer",
            "minimum": 0,
            "description": "Background spawn to cancel when action is cancel."
        }),
    );
    json!({
        "type": "object",
        "properties": properties,
        "oneOf": [
            {"properties": {"action": {"enum": ["run"]}}, "required": run_required},
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
            "unknown subagent action `{other}`; expected run, list, cancel, or cancel_all"
        ))),
    })
}
