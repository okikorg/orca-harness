//! Workflow overlay: ordinary detached agents driven by synchronous completions.
use crate::SubagentTool;
use async_trait::async_trait;
use orca_harness_core::{Concurrency, Model, Tool, ToolContext, ToolError, ToolSchema};
use orca_harness_dag::{Dag, RunId};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
mod runtime;
mod store;
use runtime::Runtime;
pub use store::WorkflowStore;

/// A depth-zero workflow tool sharing its subagent's routing, tools, admission,
/// extensions, cancellation, telemetry, and notification callback.
///
/// The host must settle UI notifications with `spawn.run.is_some()` without
/// delivering them to the parent. This overlay advances them through the same
/// callback; only the run-level notification (`run: None`) enters the inbox.
/// `store` holds stage outputs for the session and is shared with every other
/// tool the host builds from the same manager, so a run survives a model
/// switch that rebuilds this tool.
pub struct WorkflowTool<M: Model + Clone + 'static> {
    runtime: Arc<Runtime<M>>,
    last_list: Mutex<Option<Value>>,
    _owner: Arc<SubagentTool<M>>,
}
impl<M: Model + Clone + 'static> WorkflowTool<M> {
    pub fn new(subagent: Arc<SubagentTool<M>>, store: WorkflowStore) -> Result<Self, ToolError> {
        if subagent.background_config().is_none() {
            return Err(ToolError::msg(
                "workflow requires depth-zero background subagents",
            ));
        }
        Ok(Self {
            runtime: Arc::new(Runtime::new(Arc::new(subagent.workflow_worker()), store)),
            last_list: Mutex::new(None),
            _owner: subagent,
        })
    }
}
#[async_trait]
impl<M: Model + Clone + 'static> Tool for WorkflowTool<M> {
    fn schema(&self) -> ToolSchema {
        let mut stage = json!({"type":"object","additionalProperties":false,"required":["id","prompt"],"properties":{
            "id":{"type":"string"},"prompt":{"type":"string"},"needs":{"type":"array","items":{"type":"string"}},
            "kind":{"enum":["agent","map"]},"over":{"type":"string"},"schema":{"enum":["string[]","json[]"]}
        }});
        let subagent = self.runtime.subagent.schema();
        if let Some(model) = subagent.parameters.pointer("/properties/model") {
            stage["properties"]["model"] = model.clone();
        }
        if subagent
            .parameters
            .pointer("/oneOf/0/required")
            .and_then(Value::as_array)
            .is_some_and(|fields| fields.iter().any(|field| field == "model"))
        {
            stage["required"] = json!(["id", "prompt", "model"]);
        }
        ToolSchema { name:"workflow".into(), description: String::from(
            "Submit a complete dependency graph once; ordinary subagents execute its stages without sending intermediate output to your context. Use needs for dependencies, {{ stages.ID.output }} for upstream answers, kind=map with over=SOURCE and {{ item }} for array fan-out. Sources declare string[] or json[] and return strict JSON. Each stage needs a bounded self-contained task. Only terminal outputs and per-stage status/timing are delivered automatically. Never poll, sleep, wait, or repeatedly list to await results; continue useful work or end your turn. list is a user-requested snapshot; unchanged snapshots are refused. output reads one stage explicitly. cancel stops one run. Optional resumeFrom reuses matching outputs from that prior run; use only when its external inputs are still valid. maxStages bounds submitted and expanded stages (default 256). timeoutSeconds bounds the whole run including queue time. Model routing and host approvals are identical to subagent. Use only the model choices exposed in the stage schema; omit model when none is exposed."
        ), parameters: json!({"type":"object","additionalProperties":false,"required":["action"],"properties":{
            "action":{"enum":["run","list","cancel","output"]},"graph":{"type":"array","items":stage},
            "runId":{"type":"integer","minimum":0},"stage":{"type":"string"},"resumeFrom":{"type":"integer","minimum":0},
            "maxStages":{"type":"integer","minimum":1},"timeoutSeconds":{"type":"integer","minimum":1}
        },"oneOf":[
            {"properties":{"action":{"const":"run"}},"required":["graph"]},
            {"properties":{"action":{"const":"list"}}},
            {"properties":{"action":{"const":"cancel"}},"required":["runId"]},
            {"properties":{"action":{"const":"output"}},"required":["runId","stage"]}
        ]}) }
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        Concurrency::Parallel
    }
    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        match input.get("action").and_then(Value::as_str) {
            Some("run") => {
                let graph = input
                    .get("graph")
                    .and_then(Value::as_array)
                    .ok_or_else(|| ToolError::msg("graph array is required"))?;
                let cap = match input.get("maxStages") {
                    Some(v) => v
                        .as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .filter(|n| *n > 0)
                        .ok_or_else(|| ToolError::msg("maxStages must be a positive integer"))?,
                    None => orca_harness_dag::DEFAULT_STAGE_CAP,
                };
                let dag = Dag::with_cap(graph, cap).map_err(|e| ToolError::msg(e.to_string()))?;
                for stage in dag.stages() {
                    self.runtime
                        .subagent
                        .validate_model(stage.model.as_deref())?;
                }
                let resume = input
                    .get("resumeFrom")
                    .map(|v| {
                        v.as_u64()
                            .ok_or_else(|| ToolError::msg("resumeFrom must be a run id"))
                    })
                    .transpose()?;
                let timeout = input
                    .get("timeoutSeconds")
                    .map(|v| {
                        v.as_u64()
                            .filter(|n| *n > 0)
                            .ok_or_else(|| ToolError::msg("timeoutSeconds must be positive"))
                    })
                    .transpose()?;
                self.runtime.submit(dag, ctx, resume, timeout)
            }
            Some("list") => {
                let snapshot = self.runtime.list();
                let mut last = self.last_list.lock().unwrap();
                if last.as_ref() == Some(&snapshot) {
                    return Err(ToolError::msg(
                        "workflow list unchanged; do not poll. Results arrive automatically.",
                    ));
                }
                *last = Some(snapshot.clone());
                Ok(snapshot)
            }
            Some("cancel") => {
                let id = run_id(&input)?;
                self.runtime.cancel(id)?;
                Ok(json!({"runId":id,"status":"cancelled"}))
            }
            Some("output") => {
                let id = run_id(&input)?;
                let stage = input
                    .get("stage")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError::msg("stage is required"))?;
                self.runtime.output(id, stage)
            }
            _ => Err(ToolError::msg(
                "action must be run, list, cancel, or output; polling/wait is unsupported",
            )),
        }
    }
}
fn run_id(input: &Value) -> Result<RunId, ToolError> {
    input
        .get("runId")
        .and_then(Value::as_u64)
        .ok_or_else(|| ToolError::msg("runId is required"))
}
