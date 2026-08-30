//! Orcacode client-extension hooks for Agent Plugin packages.
//!
//! Agent Plugins 1.0 does not define portable hooks. Orcacode owns this
//! process protocol under `io.github.okikorg.orcacode`; the core Agent Plugin
//! manifest, Skills, and MCP components remain portable and independent.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use orca_harness_core::{
    Context, Extension, ExtensionError, HarnessError, ModelResponse, Subscriptions, ToolCall,
    ToolDecision, ToolResult,
};

use crate::agent_plugins::{PluginHook, PluginHookEvent};
use crate::mcp::apply_stdio_launch;

const PROTOCOL_VERSION: u64 = 1;
const MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;

#[derive(Clone)]
pub struct PluginHookExtension {
    hooks: Arc<[PluginHook]>,
    subscriptions: Subscriptions,
}

impl PluginHookExtension {
    pub fn new(hooks: Vec<PluginHook>) -> Self {
        let mut subscriptions = Subscriptions::none();
        for hook in &hooks {
            subscriptions = subscribe(subscriptions, hook.event);
        }
        Self {
            hooks: hooks.into(),
            subscriptions,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    fn matching(&self, event: PluginHookEvent) -> impl Iterator<Item = &PluginHook> {
        self.hooks.iter().filter(move |hook| hook.event == event)
    }
}

#[async_trait]
impl Extension for PluginHookExtension {
    fn name(&self) -> &str {
        "agent-plugin-hooks"
    }

    fn subscriptions(&self) -> Subscriptions {
        self.subscriptions
    }

    async fn on_agent_start(&self, context: &mut Context) -> Result<(), ExtensionError> {
        self.observe(
            PluginHookEvent::OnAgentStart,
            json!({ "messages": context.messages() }),
        )
        .await
    }

    async fn before_model(&self, context: &mut Context) -> Result<(), ExtensionError> {
        self.observe(
            PluginHookEvent::BeforeModel,
            json!({ "messages": context.messages() }),
        )
        .await
    }

    async fn after_model(
        &self,
        context: &mut Context,
        response: &ModelResponse,
    ) -> Result<(), ExtensionError> {
        self.observe(
            PluginHookEvent::AfterModel,
            json!({
                "messages": context.messages(),
                "response": model_response_value(response),
            }),
        )
        .await
    }

    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        let mut current = call.clone();
        let mut rewritten = false;
        for hook in self.matching(PluginHookEvent::BeforeTool) {
            let response = invoke(hook, json!({ "tool_call": current }))
                .await
                .map_err(extension_error)?;
            match parse_before_tool_response(hook, response).map_err(extension_error)? {
                ToolDecision::Continue => {}
                ToolDecision::Rewrite(arguments) => {
                    current.arguments = arguments;
                    rewritten = true;
                }
                decision @ ToolDecision::Deny { .. } => return Ok(decision),
            }
        }
        Ok(if rewritten {
            ToolDecision::Rewrite(current.arguments)
        } else {
            ToolDecision::Continue
        })
    }

    async fn after_tool(
        &self,
        call: &ToolCall,
        mut result: ToolResult,
    ) -> Result<ToolResult, ExtensionError> {
        for hook in self.matching(PluginHookEvent::AfterTool) {
            let response = invoke(hook, json!({ "tool_call": call, "result": result }))
                .await
                .map_err(extension_error)?;
            apply_after_tool_response(hook, response, &mut result).map_err(extension_error)?;
        }
        Ok(result)
    }

    async fn on_error(&self, error: &HarnessError) {
        for hook in self.matching(PluginHookEvent::OnError) {
            let _ = invoke(hook, json!({ "error": error.to_string() })).await;
        }
    }

    async fn on_agent_end(&self, context: &Context) {
        for hook in self.matching(PluginHookEvent::OnAgentEnd) {
            let _ = invoke(hook, json!({ "messages": context.messages() })).await;
        }
    }
}

impl PluginHookExtension {
    async fn observe(&self, event: PluginHookEvent, payload: Value) -> Result<(), ExtensionError> {
        for hook in self.matching(event) {
            let response = invoke(hook, payload.clone())
                .await
                .map_err(extension_error)?;
            if response.is_some_and(|value| !value.is_empty()) {
                return Err(extension_error(HookError::new(
                    hook,
                    "this hook event does not accept response fields",
                )));
            }
        }
        Ok(())
    }
}

/// Execute one hook with a representative payload and validate its response.
/// Used only by the explicit `plugin test` command.
pub async fn probe_hook(hook: &PluginHook) -> Result<(), String> {
    let payload = match hook.event {
        PluginHookEvent::OnAgentStart
        | PluginHookEvent::BeforeModel
        | PluginHookEvent::OnAgentEnd => json!({ "messages": [] }),
        PluginHookEvent::AfterModel => json!({
            "messages": [],
            "response": { "type": "final", "text": "hook probe" },
        }),
        PluginHookEvent::BeforeTool => json!({
            "tool_call": { "id": "hook-probe", "name": "hook_probe", "arguments": {} },
        }),
        PluginHookEvent::AfterTool => json!({
            "tool_call": { "id": "hook-probe", "name": "hook_probe", "arguments": {} },
            "result": {
                "call_id": "hook-probe", "tool_name": "hook_probe",
                "output": { "ok": true }, "is_error": false,
            },
        }),
        PluginHookEvent::OnError => json!({ "error": "hook probe" }),
    };
    let response = invoke(hook, payload)
        .await
        .map_err(|error| error.to_string())?;
    match hook.event {
        PluginHookEvent::BeforeTool => {
            parse_before_tool_response(hook, response).map_err(|error| error.to_string())?;
        }
        PluginHookEvent::AfterTool => {
            let mut result = ToolResult {
                call_id: "hook-probe".into(),
                tool_name: "hook_probe".into(),
                output: json!({ "ok": true }),
                is_error: false,
            };
            apply_after_tool_response(hook, response, &mut result)
                .map_err(|error| error.to_string())?;
        }
        _ if response.is_some_and(|value| !value.is_empty()) => {
            return Err(
                HookError::new(hook, "this hook event does not accept response fields").to_string(),
            );
        }
        _ => {}
    }
    Ok(())
}

async fn invoke(
    hook: &PluginHook,
    payload: Value,
) -> Result<Option<Map<String, Value>>, HookError> {
    let input = serde_json::to_vec(&json!({
        "version": PROTOCOL_VERSION,
        "event": hook.event.as_str(),
        "plugin": hook.plugin_name,
        "payload": payload,
    }))
    .map_err(|error| HookError::new(hook, format!("cannot encode input: {error}")))?;
    if input.len() > MAX_INPUT_BYTES {
        return Err(HookError::new(
            hook,
            format!("input exceeds {MAX_INPUT_BYTES} bytes"),
        ));
    }

    let mut command = Command::new(&hook.launch.command);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    apply_stdio_launch(&mut command, &hook.launch);
    let mut child = command
        .spawn()
        .map_err(|error| HookError::new(hook, format!("spawn failed: {error}")))?;
    let mut stdin = child.stdin.take().expect("hook stdin piped");
    let mut stdout = child.stdout.take().expect("hook stdout piped");
    let mut stderr = child.stderr.take().expect("hook stderr piped");

    let run = async {
        stdin.write_all(&input).await?;
        stdin.write_all(b"\n").await?;
        stdin.shutdown().await?;
        let read_stdout = read_limited(&mut stdout);
        let read_stderr = read_limited(&mut stderr);
        let (status, stdout, stderr) = tokio::join!(child.wait(), read_stdout, read_stderr);
        Ok::<_, std::io::Error>((status?, stdout?, stderr?))
    };
    let (status, stdout, stderr) = match tokio::time::timeout(hook.timeout, run).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Err(HookError::new(hook, format!("I/O failed: {error}"))),
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(HookError::new(
                hook,
                format!("timed out after {} ms", hook.timeout.as_millis()),
            ));
        }
    };
    if !status.success() {
        let diagnostic = String::from_utf8_lossy(&stderr).trim().to_owned();
        let suffix = if diagnostic.is_empty() {
            String::new()
        } else {
            format!(": {diagnostic}")
        };
        return Err(HookError::new(
            hook,
            format!("exited with {status}{suffix}"),
        ));
    }
    if stdout.len() as u64 > MAX_OUTPUT_BYTES {
        return Err(HookError::new(
            hook,
            format!("stdout exceeds {MAX_OUTPUT_BYTES} bytes"),
        ));
    }
    let text = std::str::from_utf8(&stdout)
        .map_err(|error| HookError::new(hook, format!("stdout is not UTF-8: {error}")))?
        .trim();
    if text.is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|error| HookError::new(hook, format!("stdout is not one JSON object: {error}")))?;
    value
        .as_object()
        .cloned()
        .map(Some)
        .ok_or_else(|| HookError::new(hook, "stdout JSON must be an object"))
}

async fn read_limited(reader: &mut (impl AsyncReadExt + Unpin)) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    reader
        .take(MAX_OUTPUT_BYTES + 1)
        .read_to_end(&mut output)
        .await?;
    Ok(output)
}

fn parse_before_tool_response(
    hook: &PluginHook,
    response: Option<Map<String, Value>>,
) -> Result<ToolDecision, HookError> {
    let Some(mut response) = response else {
        return Ok(ToolDecision::Continue);
    };
    if response.is_empty() {
        return Ok(ToolDecision::Continue);
    }
    let decision = response
        .remove("decision")
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| HookError::new(hook, "response decision must be a string"))?;
    match decision.as_str() {
        "continue" if response.is_empty() => Ok(ToolDecision::Continue),
        "deny" => {
            let reason = response
                .remove("reason")
                .and_then(|value| value.as_str().map(str::to_owned))
                .filter(|value| !value.is_empty())
                .ok_or_else(|| HookError::new(hook, "deny response requires a reason"))?;
            ensure_empty(hook, &response)?;
            Ok(ToolDecision::Deny { reason })
        }
        "rewrite" => {
            let arguments = response
                .remove("arguments")
                .ok_or_else(|| HookError::new(hook, "rewrite response requires arguments"))?;
            ensure_empty(hook, &response)?;
            Ok(ToolDecision::Rewrite(arguments))
        }
        "continue" => Err(HookError::new(hook, "continue response has unknown fields")),
        other => Err(HookError::new(
            hook,
            format!("unknown before_tool decision {other}"),
        )),
    }
}

fn apply_after_tool_response(
    hook: &PluginHook,
    response: Option<Map<String, Value>>,
    result: &mut ToolResult,
) -> Result<(), HookError> {
    let Some(mut response) = response else {
        return Ok(());
    };
    if let Some(output) = response.remove("output") {
        result.output = output;
    }
    if let Some(is_error) = response.remove("is_error") {
        result.is_error = is_error
            .as_bool()
            .ok_or_else(|| HookError::new(hook, "is_error must be a boolean"))?;
    }
    ensure_empty(hook, &response)
}

fn ensure_empty(hook: &PluginHook, response: &Map<String, Value>) -> Result<(), HookError> {
    if let Some(field) = response.keys().next() {
        Err(HookError::new(
            hook,
            format!("unknown response field {field}"),
        ))
    } else {
        Ok(())
    }
}

fn subscribe(mut subscriptions: Subscriptions, event: PluginHookEvent) -> Subscriptions {
    match event {
        PluginHookEvent::OnAgentStart => subscriptions.on_agent_start = true,
        PluginHookEvent::BeforeModel => subscriptions.before_model = true,
        PluginHookEvent::AfterModel => subscriptions.after_model = true,
        PluginHookEvent::BeforeTool => subscriptions.before_tool = true,
        PluginHookEvent::AfterTool => subscriptions.after_tool = true,
        PluginHookEvent::OnError => subscriptions.on_error = true,
        PluginHookEvent::OnAgentEnd => subscriptions.on_agent_end = true,
    }
    subscriptions
}

fn model_response_value(response: &ModelResponse) -> Value {
    match response {
        ModelResponse::Final { text, usage } => {
            json!({ "type": "final", "text": text, "usage": usage })
        }
        ModelResponse::ToolCalls {
            content,
            calls,
            usage,
        } => json!({
            "type": "tool_calls", "content": content, "calls": calls, "usage": usage,
        }),
    }
}

#[derive(Debug)]
struct HookError {
    plugin: String,
    event: PluginHookEvent,
    index: usize,
    message: String,
}

impl HookError {
    fn new(hook: &PluginHook, message: impl Into<String>) -> Self {
        Self {
            plugin: hook.plugin_name.clone(),
            event: hook.event,
            index: hook.index,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for HookError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "plugin {} {} hook #{}: {}",
            self.plugin,
            self.event.as_str(),
            self.index + 1,
            self.message
        )
    }
}

fn extension_error(error: HookError) -> ExtensionError {
    ExtensionError::new("agent-plugin-hooks", error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use orca_harness_core::Extension;

    use super::*;
    use crate::mcp::{ProcessEnvironment, StdioLaunch};

    #[cfg(unix)]
    fn shell_hook(event: PluginHookEvent, script: &str, timeout_ms: u64) -> PluginHook {
        PluginHook {
            plugin_name: "test-plugin".into(),
            event,
            index: 0,
            launch: StdioLaunch {
                command: "/bin/sh".into(),
                args: vec!["-c".into(), script.into()],
                env: BTreeMap::new(),
                cwd: None,
                environment: ProcessEnvironment::Sanitized,
            },
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: "write_file".into(),
            arguments: json!({ "path": "before" }),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn before_tool_hooks_can_rewrite_then_deny() {
        let rewrite = shell_hook(
            PluginHookEvent::BeforeTool,
            "read input; printf '%s' '{\"decision\":\"rewrite\",\"arguments\":{\"path\":\"after\"}}'",
            1_000,
        );
        let deny = shell_hook(
            PluginHookEvent::BeforeTool,
            "read input; case \"$input\" in *after*) printf '%s' '{\"decision\":\"deny\",\"reason\":\"blocked\"}';; *) exit 7;; esac",
            1_000,
        );
        let extension = PluginHookExtension::new(vec![rewrite, deny]);

        let decision = extension.before_tool(&call()).await.unwrap();

        assert!(matches!(decision, ToolDecision::Deny { reason } if reason == "blocked"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn after_tool_hook_can_replace_only_output_and_error_flag() {
        let hook = shell_hook(
            PluginHookEvent::AfterTool,
            "read input; printf '%s' '{\"output\":{\"redacted\":true},\"is_error\":true}'",
            1_000,
        );
        let extension = PluginHookExtension::new(vec![hook]);
        let original_call = call();
        let result = ToolResult::ok(&original_call, json!({ "secret": "value" }));

        let transformed = extension.after_tool(&original_call, result).await.unwrap();

        assert_eq!(transformed.call_id, "call-1");
        assert_eq!(transformed.tool_name, "write_file");
        assert_eq!(transformed.output, json!({ "redacted": true }));
        assert!(transformed.is_error);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hook_timeout_is_bounded_and_terminal_for_deterministic_events() {
        let hook = shell_hook(PluginHookEvent::BeforeModel, "sleep 1", 10);
        let extension = PluginHookExtension::new(vec![hook]);

        let error = extension
            .before_model(&mut Context::new())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("timed out"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hook_process_does_not_inherit_unrelated_environment_secrets() {
        const SECRET: &str = "ORCACODE_PLUGIN_HOOK_TEST_SECRET";
        std::env::set_var(SECRET, "must-not-leak");
        let hook = shell_hook(
            PluginHookEvent::BeforeModel,
            "read input; [ -z \"$ORCACODE_PLUGIN_HOOK_TEST_SECRET\" ]",
            1_000,
        );
        let result = PluginHookExtension::new(vec![hook])
            .before_model(&mut Context::new())
            .await;
        std::env::remove_var(SECRET);

        result.unwrap();
    }
}
