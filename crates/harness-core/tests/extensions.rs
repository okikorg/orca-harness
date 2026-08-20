//! Extension lifecycle: hook ordering, policy decisions, wrapping, and
//! the compiled-subscription fast path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::time::timeout;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    Agent, Context, Extension, ExtensionError, FnTool, HarnessError, Message, ModelResponse, Next,
    Subscriptions, ToolCall, ToolContext, ToolDecision, ToolError, ToolResult,
};

const RUN_TIMEOUT: Duration = Duration::from_secs(10);

type Log = Arc<Mutex<Vec<String>>>;

fn log_of(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

struct Recorder {
    log: Log,
}

#[async_trait]
impl Extension for Recorder {
    fn name(&self) -> &str {
        "recorder"
    }

    async fn on_agent_start(&self, _context: &mut Context) -> Result<(), ExtensionError> {
        self.log.lock().unwrap().push("on_agent_start".into());
        Ok(())
    }
    async fn before_model(&self, _context: &mut Context) -> Result<(), ExtensionError> {
        self.log.lock().unwrap().push("before_model".into());
        Ok(())
    }
    async fn after_model(
        &self,
        _context: &mut Context,
        _response: &ModelResponse,
    ) -> Result<(), ExtensionError> {
        self.log.lock().unwrap().push("after_model".into());
        Ok(())
    }
    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("before_tool:{}", call.id));
        Ok(ToolDecision::Continue)
    }
    async fn around_tool<'a>(
        &self,
        call: &ToolCall,
        input: Value,
        _ctx: &ToolContext,
        next: Next<'a>,
    ) -> Result<Value, ToolError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("around_tool:enter:{}", call.id));
        let out = next.run(input).await;
        self.log
            .lock()
            .unwrap()
            .push(format!("around_tool:exit:{}", call.id));
        out
    }
    async fn after_tool(
        &self,
        call: &ToolCall,
        result: ToolResult,
    ) -> Result<ToolResult, ExtensionError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("after_tool:{}", call.id));
        Ok(result)
    }
    async fn tool_result(&self, result: &ToolResult) {
        self.log
            .lock()
            .unwrap()
            .push(format!("tool_result:{}", result.call_id));
    }
    async fn on_error(&self, _error: &HarnessError) {
        self.log.lock().unwrap().push("on_error".into());
    }
    async fn on_agent_end(&self, _context: &Context) {
        self.log.lock().unwrap().push("on_agent_end".into());
    }
}

fn echo_tool() -> FnTool {
    FnTool::new(
        "echo",
        "echoes input",
        json!({"type": "object"}),
        |input, _ctx| async move { Ok(input) },
    )
}

#[tokio::test]
async fn lifecycle_hooks_fire_in_order() {
    let log: Log = Arc::default();
    let model = ScriptedModel::tool_round(vec![call("call_0", "echo", json!({}))], "done");
    let agent = Agent::new(model)
        .tool(echo_tool())
        .extension(Recorder { log: log.clone() });
    timeout(RUN_TIMEOUT, agent.run("go"))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        log_of(&log),
        vec![
            "on_agent_start",
            "before_model",
            "after_model",
            "before_tool:call_0",
            "around_tool:enter:call_0",
            "around_tool:exit:call_0",
            "after_tool:call_0",
            "tool_result:call_0",
            "before_model",
            "after_model",
            "on_agent_end",
        ]
    );
}

#[tokio::test]
async fn on_error_fires_before_agent_end_on_failure() {
    let log: Log = Arc::default();
    let model = ScriptedModel::new(vec![]); // model error on first step
    let agent = Agent::new(model).extension(Recorder { log: log.clone() });
    let result = timeout(RUN_TIMEOUT, agent.run("fail")).await.unwrap();
    assert!(result.is_err());

    let entries = log_of(&log);
    assert_eq!(
        entries,
        vec!["on_agent_start", "before_model", "on_error", "on_agent_end"]
    );
}

struct DenyPolicy;

#[async_trait]
impl Extension for DenyPolicy {
    fn name(&self) -> &str {
        "deny-policy"
    }
    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().before_tool()
    }
    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        if call.name == "dangerous" {
            Ok(ToolDecision::Deny {
                reason: "not allowed".into(),
            })
        } else {
            Ok(ToolDecision::Continue)
        }
    }
}

#[tokio::test]
async fn deny_blocks_execution_but_run_continues() {
    let ran = Arc::new(AtomicBool::new(false));
    let dangerous = {
        let ran = ran.clone();
        FnTool::new(
            "dangerous",
            "must not run",
            json!({"type": "object"}),
            move |_input, _ctx| {
                let ran = ran.clone();
                async move {
                    ran.store(true, Ordering::SeqCst);
                    Ok(Value::Null)
                }
            },
        )
    };

    let model = Arc::new(ScriptedModel::tool_round(
        vec![
            call("call_0", "dangerous", json!({})),
            call("call_1", "echo", json!({"safe": true})),
        ],
        "done",
    ));
    let agent = Agent::new(model.clone())
        .tool(dangerous)
        .tool(echo_tool())
        .extension(DenyPolicy);
    let answer = timeout(RUN_TIMEOUT, agent.run("try"))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(answer, "done");
    assert!(
        !ran.load(Ordering::SeqCst),
        "denied tool must never execute"
    );

    let seen = model.observed_contexts();
    let results = seen[1]
        .messages()
        .iter()
        .find_map(|m| match m {
            Message::Tool { results } => Some(results.clone()),
            _ => None,
        })
        .unwrap();
    assert!(results[0].is_error);
    assert!(results[0].output["error"]
        .as_str()
        .unwrap()
        .contains("not allowed"));
    assert_eq!(
        results[1].output,
        json!({"safe": true}),
        "sibling call unaffected"
    );
}

struct RewriteArgs;

#[async_trait]
impl Extension for RewriteArgs {
    fn name(&self) -> &str {
        "rewrite-args"
    }
    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().before_tool()
    }
    async fn before_tool(&self, _call: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        Ok(ToolDecision::Rewrite(json!({"x": 2})))
    }
}

#[tokio::test]
async fn before_tool_can_rewrite_arguments() {
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("call_0", "echo", json!({"x": 1}))],
        "done",
    ));
    let agent = Agent::new(model.clone())
        .tool(echo_tool())
        .extension(RewriteArgs);
    timeout(RUN_TIMEOUT, agent.run("rewrite"))
        .await
        .unwrap()
        .unwrap();

    let seen = model.observed_contexts();
    let results = seen[1]
        .messages()
        .iter()
        .find_map(|m| match m {
            Message::Tool { results } => Some(results.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        results[0].output,
        json!({"x": 2}),
        "tool received rewritten arguments"
    );
}

/// A wrapper that tags the output, proving `around_tool` composes in
/// registration order (first registered = outermost).
struct Wrapper {
    tag: &'static str,
}

#[async_trait]
impl Extension for Wrapper {
    fn name(&self) -> &str {
        self.tag
    }
    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().around_tool()
    }
    async fn around_tool<'a>(
        &self,
        _call: &ToolCall,
        input: Value,
        _ctx: &ToolContext,
        next: Next<'a>,
    ) -> Result<Value, ToolError> {
        let inner = next.run(input).await?;
        Ok(json!({"tag": self.tag, "inner": inner}))
    }
}

#[tokio::test]
async fn around_tool_wraps_in_registration_order() {
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("call_0", "echo", json!({"v": 1}))],
        "done",
    ));
    let agent = Agent::new(model.clone())
        .tool(echo_tool())
        .extension(Wrapper { tag: "outer" })
        .extension(Wrapper { tag: "inner" });
    timeout(RUN_TIMEOUT, agent.run("wrap"))
        .await
        .unwrap()
        .unwrap();

    let seen = model.observed_contexts();
    let results = seen[1]
        .messages()
        .iter()
        .find_map(|m| match m {
            Message::Tool { results } => Some(results.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        results[0].output,
        json!({"tag": "outer", "inner": {"tag": "inner", "inner": {"v": 1}}}),
    );
}

/// An around_tool wrapper acting as a per-call timeout: replaces execution
/// without becoming the Dispatcher.
struct ToolTimeout {
    limit: Duration,
}

#[async_trait]
impl Extension for ToolTimeout {
    fn name(&self) -> &str {
        "tool-timeout"
    }
    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().around_tool()
    }
    async fn around_tool<'a>(
        &self,
        _call: &ToolCall,
        input: Value,
        _ctx: &ToolContext,
        next: Next<'a>,
    ) -> Result<Value, ToolError> {
        tokio::time::timeout(self.limit, next.run(input))
            .await
            .map_err(|_| ToolError::msg("tool timed out"))?
    }
}

#[tokio::test]
async fn around_tool_timeout_normalizes_to_error_result() {
    let slow = FnTool::new(
        "slow",
        "sleeps",
        json!({"type": "object"}),
        |_input, _ctx| async move {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(Value::Null)
        },
    );
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("call_0", "slow", json!({}))],
        "recovered",
    ));
    let agent = Agent::new(model.clone()).tool(slow).extension(ToolTimeout {
        limit: Duration::from_millis(100),
    });
    let answer = timeout(RUN_TIMEOUT, agent.run("timeout"))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(answer, "recovered");
    let seen = model.observed_contexts();
    let results = seen[1]
        .messages()
        .iter()
        .find_map(|m| match m {
            Message::Tool { results } => Some(results.clone()),
            _ => None,
        })
        .unwrap();
    assert!(results[0].is_error);
    assert_eq!(results[0].output["error"], json!("tool timed out"));
}

struct Truncator;

#[async_trait]
impl Extension for Truncator {
    fn name(&self) -> &str {
        "truncator"
    }
    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().after_tool()
    }
    async fn after_tool(
        &self,
        _call: &ToolCall,
        mut result: ToolResult,
    ) -> Result<ToolResult, ExtensionError> {
        if let Some(text) = result.output["text"].as_str() {
            if text.len() > 8 {
                result.output["text"] = json!(format!("{}…", &text[..8]));
            }
        }
        Ok(result)
    }
}

#[tokio::test]
async fn after_tool_transforms_results() {
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call(
            "call_0",
            "echo",
            json!({"text": "aaaaaaaaaaaaaaaaaaaa"}),
        )],
        "done",
    ));
    let agent = Agent::new(model.clone())
        .tool(echo_tool())
        .extension(Truncator);
    timeout(RUN_TIMEOUT, agent.run("truncate"))
        .await
        .unwrap()
        .unwrap();

    let seen = model.observed_contexts();
    let results = seen[1]
        .messages()
        .iter()
        .find_map(|m| match m {
            Message::Tool { results } => Some(results.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(results[0].output["text"], json!("aaaaaaaa…"));
}

/// An extension whose hooks would record — but with no subscriptions, the
/// compiled registry must never call them.
struct Unsubscribed {
    log: Log,
}

#[async_trait]
impl Extension for Unsubscribed {
    fn name(&self) -> &str {
        "unsubscribed"
    }
    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none()
    }
    async fn before_model(&self, _context: &mut Context) -> Result<(), ExtensionError> {
        self.log.lock().unwrap().push("should never happen".into());
        Ok(())
    }
    async fn before_tool(&self, _call: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        self.log.lock().unwrap().push("should never happen".into());
        Ok(ToolDecision::Continue)
    }
}

#[tokio::test]
async fn unsubscribed_extensions_are_never_invoked() {
    let log: Log = Arc::default();
    let model = ScriptedModel::tool_round(vec![call("call_0", "echo", json!({}))], "done");
    let agent = Agent::new(model)
        .tool(echo_tool())
        .extension(Unsubscribed { log: log.clone() });
    timeout(RUN_TIMEOUT, agent.run("fast path"))
        .await
        .unwrap()
        .unwrap();

    assert!(
        log_of(&log).is_empty(),
        "compiled dispatch must skip unsubscribed hooks"
    );
}

struct FailingHook;

#[async_trait]
impl Extension for FailingHook {
    fn name(&self) -> &str {
        "failing-hook"
    }
    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().before_model()
    }
    async fn before_model(&self, _context: &mut Context) -> Result<(), ExtensionError> {
        Err(ExtensionError::new("failing-hook", "boom"))
    }
}

#[tokio::test]
async fn extension_hook_failure_is_terminal() {
    let model = ScriptedModel::new(vec![ModelResponse::final_text("unreachable")]);
    let agent = Agent::new(model).extension(FailingHook);
    let result = timeout(RUN_TIMEOUT, agent.run("boom")).await.unwrap();
    assert!(
        matches!(result, Err(HarnessError::Extension(_))),
        "got: {result:?}"
    );
}
