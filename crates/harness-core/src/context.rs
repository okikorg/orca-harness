//! Context is model-visible state, not durable memory. Durable memory is
//! an Extension concern.

use serde::{Deserialize, Serialize};

use crate::tool::{ToolCall, ToolResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        results: Vec<ToolResult>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Context {
    messages: Vec<Message>,
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    pub fn push_system(&mut self, content: impl Into<String>) {
        self.messages.push(Message::System {
            content: content.into(),
        });
    }

    pub fn push_user(&mut self, content: impl Into<String>) {
        self.messages.push(Message::User {
            content: content.into(),
        });
    }

    pub fn push_assistant_text(&mut self, content: impl Into<String>) {
        self.messages.push(Message::Assistant {
            content: Some(content.into()),
            tool_calls: Vec::new(),
        });
    }

    pub fn push_assistant_tool_calls(&mut self, content: Option<String>, calls: Vec<ToolCall>) {
        self.messages.push(Message::Assistant {
            content,
            tool_calls: calls,
        });
    }

    pub fn append_tool_results(&mut self, results: Vec<ToolResult>) {
        self.messages.push(Message::Tool { results });
    }
}
