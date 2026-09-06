//! A skill loads once per conversation.
//!
//! The `skill` tool is stateless and its catalog is re-read every turn,
//! so a model that sees "call this before starting work" reloads the
//! same always-on skill at the top of each turn: a round trip, and a
//! second copy of the text in context. This wrapper answers a repeat
//! with a short reminder instead.
//!
//! "Already loaded" is read off the context before every model call,
//! not kept as a session flag. Compaction elides old tool results and
//! `/clear` empties the context, and in both cases the model has really
//! lost the text — the next load must be a real one. Deriving the set
//! from what the model can currently see makes that automatic, with
//! nothing for the host to reset. It also means this extension must be
//! registered after any compaction extension, so it scans the context
//! the model is about to receive rather than the one compaction is
//! about to shrink.

use std::collections::HashSet;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{json, Value};

use orca_harness_core::{
    Context, Extension, ExtensionError, Message, Next, Subscriptions, ToolCall, ToolContext,
    ToolError,
};

const TOOL_NAME: &str = "skill";

#[derive(Default)]
pub struct SkillOnce {
    /// Skills whose `SKILL.md` body is in the context, by name. Rebuilt
    /// before each model call; claimed on the way into a load so two
    /// calls for one skill in the same batch produce one load.
    loaded: Mutex<HashSet<String>>,
}

impl SkillOnce {
    pub fn new() -> Self {
        Self::default()
    }

    fn stub(name: &str) -> Value {
        json!({
            "name": name,
            "alreadyLoaded": true,
            "note": format!(
                "{name} is already loaded in this conversation; its instructions still \
                 apply and were not re-sent."
            ),
        })
    }
}

/// The skill whose body `input` asks for from the top. A `resource`
/// read or a continuation page is never a repeat of the body.
fn body_request(input: &Value) -> Option<&str> {
    let resource = input.get("resource").and_then(Value::as_str);
    if resource.is_some_and(|rel| !rel.is_empty()) {
        return None;
    }
    if input.get("offset").and_then(Value::as_u64).unwrap_or(0) != 0 {
        return None;
    }
    input.get("name").and_then(Value::as_str)
}

/// The skill whose body `output` still carries. A resource read names a
/// different file, and an elided result keeps a hint but loses the
/// `instructions` field — neither means the body is in context.
fn loaded_body(output: &Value) -> Option<&str> {
    if output.get("resource").is_some() || !output.get("instructions").is_some_and(Value::is_string)
    {
        return None;
    }
    output.get("name").and_then(Value::as_str)
}

#[async_trait]
impl Extension for SkillOnce {
    fn name(&self) -> &str {
        "skill-once"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().before_model().around_tool()
    }

    async fn before_model(&self, context: &mut Context) -> Result<(), ExtensionError> {
        let mut loaded = self.loaded.lock().expect("skill-once lock");
        loaded.clear();
        for message in context.messages() {
            let Message::Tool { results } = message else {
                continue;
            };
            for result in results {
                if result.tool_name != TOOL_NAME || result.is_error {
                    continue;
                }
                if let Some(name) = loaded_body(&result.output) {
                    loaded.insert(name.to_string());
                }
            }
        }
        Ok(())
    }

    async fn around_tool<'a>(
        &self,
        call: &ToolCall,
        input: Value,
        _ctx: &ToolContext,
        next: Next<'a>,
    ) -> Result<Value, ToolError> {
        if call.name != TOOL_NAME {
            return next.run(input).await;
        }
        let Some(name) = body_request(&input).map(str::to_string) else {
            return next.run(input).await;
        };
        if !self
            .loaded
            .lock()
            .expect("skill-once lock")
            .insert(name.clone())
        {
            return Ok(Self::stub(&name));
        }
        let result = next.run(input).await;
        if result.is_err() {
            // An unknown name or an unreadable file put nothing in
            // context; a corrected retry must load for real.
            self.loaded.lock().expect("skill-once lock").remove(&name);
        }
        result
    }
}

#[cfg(test)]
mod tests;
