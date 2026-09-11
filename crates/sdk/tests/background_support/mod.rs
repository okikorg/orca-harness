//! Fixtures shared by the background subagent suites: a worker model
//! that waits to be released, a hook that ignores cancellation, bounded
//! waits over the notification channel, and the routed parent/child
//! agent the delivery tests run on. Kept out of `common` so that module
//! stays std-only for `consumer_api.rs`.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    CancellationToken, Context, Extension, ExtensionError, Message, Model, ModelError,
    ModelResponse, Subscriptions, ToolSchema,
};
use orca_harness_sdk::orchestration::SubagentModel;
use orca_harness_sdk::{
    Agent, BackgroundNotification, Harness, Stage, SubagentConfig, SubagentDepth,
};
use serde_json::json;
use tokio::sync::broadcast::Receiver;
use tokio::sync::Notify;

/// One ordinary workflow stage with its dependencies.
pub fn stage(id: &str, prompt: &str, needs: &[&str]) -> Stage {
    let mut stage = Stage::new(id, prompt);
    stage.needs = needs.iter().map(|need| need.to_string()).collect();
    stage
}

/// Blocks every worker until released; honours cancellation.
pub struct Held(pub CancellationToken);

#[async_trait]
impl Model for Held {
    async fn generate(&self, _: &Context, _: &[ToolSchema]) -> Result<ModelResponse, ModelError> {
        self.0.cancelled().await;
        Ok(ModelResponse::final_text("held done"))
    }
}

/// Answers every task with the task text itself, so a workflow stage's
/// output is its rendered prompt.
pub struct Echo;

#[async_trait]
impl Model for Echo {
    async fn generate(
        &self,
        context: &Context,
        _: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        let task = context
            .messages()
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("a task");
        Ok(ModelResponse::final_text(task))
    }
}

/// A child extension whose hook never looks at the worker's token: the
/// kernel guards model and tool calls with the token, hooks it does not.
/// Signals `entered` once the worker is inside the hook.
pub struct Stall(pub Arc<Notify>);

#[async_trait]
impl Extension for Stall {
    fn name(&self) -> &str {
        "stall"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().before_model()
    }

    async fn before_model(&self, _: &mut Context) -> Result<(), ExtensionError> {
        self.0.notify_one();
        tokio::time::sleep(Duration::from_secs(5)).await;
        Ok(())
    }
}

pub async fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
    for _ in 0..500 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// The next notification matching `pick`, skipping the others.
pub async fn next_matching<T>(
    receiver: &mut Receiver<BackgroundNotification>,
    what: &str,
    mut pick: impl FnMut(BackgroundNotification) -> Option<T>,
) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let notification = tokio::time::timeout_at(deadline, receiver.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .unwrap_or_else(|error| panic!("channel closed waiting for {what}: {error}"));
        if let Some(value) = pick(notification) {
            return value;
        }
    }
}

pub fn spawn_call(id: &str, task: &str) -> ModelResponse {
    ModelResponse::ToolCalls {
        content: None,
        calls: vec![call(
            id,
            "subagent",
            json!({"task": task, "background": true}),
        )],
        usage: None,
    }
}

pub fn tool_call(id: &str, tool: &str) -> ModelResponse {
    ModelResponse::ToolCalls {
        content: None,
        calls: vec![call(id, tool, json!({}))],
        usage: None,
    }
}

/// An agent whose children run on `child` through the `flash/child`
/// route; the session's route is fixed to it after opening.
pub fn routed_agent(
    harness: &Harness,
    parent: Arc<ScriptedModel>,
    child: Arc<dyn Model>,
    config: SubagentConfig,
) -> Agent {
    harness
        .agent(parent)
        .subagents(config.model(SubagentModel::new("flash/child", "the child", child)))
        .build()
        .unwrap()
}

pub fn route_to_child(settings: &SubagentDepth) {
    assert!(settings.set_default_model(Some("flash/child".into())));
}

pub fn completion_messages(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::User { content, .. }
                if content.contains("background_subagent_completions") =>
            {
                Some(content.clone())
            }
            _ => None,
        })
        .collect()
}
