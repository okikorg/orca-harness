//! Consumer API baseline.
//!
//! This fixture MUST import Orca types only from `orca_harness_sdk` (plus
//! third-party dev-dependencies: tokio, async_trait, serde_json). It stands in
//! for a downstream crate whose sole Orca dependency is the SDK, and proves
//! that such a crate can implement every core contract (`Model`, `Tool`,
//! `Extension`, `CredentialSource`, `CodexCredentialSource`) and drive a run
//! without reaching into lower-level crates. If a companion type is missing
//! from the SDK, this file fails to compile, which is the point.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use orca_harness_sdk::contracts::{Concurrency, ModelError, Next, ToolDecision, ToolName};
use orca_harness_sdk::providers::{
    CodexCredential, CodexCredentialSource, ModelInfo, OpenAiCodexModel,
};
use orca_harness_sdk::{
    BearerCredential, Context, CredentialError, CredentialSource, Extension, ExtensionError,
    Harness, HarnessError, Message, Model, ModelDelta, ModelResponse, Subscriptions, Tool,
    ToolCall, ToolContext, ToolError, ToolResult, ToolSchema, Usage,
};
use serde_json::{json, Value};

fn temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "orca-sdk-consumer-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A scripted model: first turn calls the custom tool, second turn answers
/// with whatever the tool returned.
struct ScriptedModel {
    turns: AtomicUsize,
}

#[async_trait]
impl Model for ScriptedModel {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        let turn = self.turns.fetch_add(1, Ordering::SeqCst);
        if turn == 0 {
            assert!(tools.iter().any(|schema| schema.name == "shout"));
            return Ok(ModelResponse::ToolCalls {
                content: None,
                calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "shout".into(),
                    arguments: json!({ "text": "hi" }),
                }],
                usage: Some(Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_tokens: 0,
                    cache_create_tokens: 0,
                    reasoning_tokens: None,
                }),
            });
        }
        let last_tool_output = context
            .messages()
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Tool { results } => results.first().map(|r| r.output.clone()),
                _ => None,
            })
            .expect("tool result should be in context");
        Ok(ModelResponse::final_text(format!(
            "model says: {}",
            last_tool_output["shouted"].as_str().unwrap_or("")
        )))
    }
}

/// A custom tool that uppercases its input.
struct ShoutTool;

#[async_trait]
impl Tool for ShoutTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: ToolName::from("shout"),
            description: "Uppercase the given text".into(),
            parameters: json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }),
        }
    }

    fn concurrency(&self, _input: &Value) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        assert_eq!(ctx.tool_name, "shout");
        let text = input["text"]
            .as_str()
            .ok_or_else(|| ToolError::msg("text is required"))?;
        Ok(json!({ "shouted": text.to_uppercase() }))
    }
}

/// A custom extension that observes every tool hook and streaming delta.
#[derive(Default)]
struct Observer {
    before: AtomicUsize,
    around: AtomicUsize,
    after: AtomicUsize,
    deltas: AtomicUsize,
}

#[async_trait]
impl Extension for Observer {
    fn name(&self) -> &str {
        "observer"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none()
            .before_tool()
            .around_tool()
            .after_tool()
            .model_delta()
            .on_error()
    }

    async fn on_model_delta(&self, _delta: &ModelDelta) {
        self.deltas.fetch_add(1, Ordering::SeqCst);
    }

    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        if call.name == "forbidden" {
            return Err(ExtensionError::new(self.name(), "blocked"));
        }
        self.before.fetch_add(1, Ordering::SeqCst);
        Ok(ToolDecision::Continue)
    }

    async fn around_tool<'a>(
        &self,
        _call: &ToolCall,
        input: Value,
        _ctx: &ToolContext,
        next: Next<'a>,
    ) -> Result<Value, ToolError> {
        self.around.fetch_add(1, Ordering::SeqCst);
        next.run(input).await
    }

    async fn after_tool(
        &self,
        _call: &ToolCall,
        result: ToolResult,
    ) -> Result<ToolResult, ExtensionError> {
        self.after.fetch_add(1, Ordering::SeqCst);
        Ok(result)
    }

    async fn on_error(&self, _error: &HarnessError) {}
}

/// A host-owned credential source that also satisfies the Codex contract.
struct HostCredentials;

#[async_trait]
impl CredentialSource for HostCredentials {
    async fn credential(&self) -> Result<BearerCredential, CredentialError> {
        Ok(BearerCredential {
            access_token: "token".into(),
            expires_at: None,
        })
    }

    async fn refresh(&self) -> Result<BearerCredential, CredentialError> {
        self.credential().await
    }
}

#[async_trait]
impl CodexCredentialSource for HostCredentials {
    async fn codex_credential(&self) -> Result<CodexCredential, CredentialError> {
        Ok(CodexCredential {
            bearer: self.credential().await?,
            account_id: "account".into(),
        })
    }

    async fn refresh_codex(&self, _rejected: &str) -> Result<CodexCredential, CredentialError> {
        self.codex_credential().await
    }
}

#[tokio::test]
async fn consumer_can_implement_every_contract_and_run() {
    let root = temp_dir("contracts");
    let harness = Harness::builder().workspace(&root).build().unwrap();

    let observer = Arc::new(Observer::default());
    let agent = harness
        .agent(ScriptedModel {
            turns: AtomicUsize::new(0),
        })
        .tool(ShoutTool)
        .extension_arc(observer.clone())
        .build()
        .unwrap();

    let session = agent.new_session().ephemeral().open().unwrap();
    let result = session.run("hi").await.unwrap();

    assert_eq!(result.text, "model says: HI");
    assert_eq!(observer.before.load(Ordering::SeqCst), 1);
    assert_eq!(observer.around.load(Ordering::SeqCst), 1);
    assert_eq!(observer.after.load(Ordering::SeqCst), 1);
}

#[test]
fn consumer_can_configure_codex_credentials() {
    // Construction only; no network access.
    let model = OpenAiCodexModel::new("gpt-5-codex", Arc::new(HostCredentials));
    let _model = model.reasoning_effort("low");
}

#[test]
fn consumer_can_name_catalog_and_error_types() {
    fn takes_info(_: Option<ModelInfo>) {}
    takes_info(None);

    let err: HarnessError = ToolError::msg("boom").into();
    assert!(matches!(err, HarnessError::Tool(_)));
    let err: HarnessError = ModelError::Request("down".into()).into();
    assert!(matches!(err, HarnessError::Model(_)));
}
