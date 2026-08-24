//! Context is model-visible state, not durable memory. Durable memory is
//! an Extension concern.

use serde::{Deserialize, Serialize};

use crate::tool::{ToolCall, ToolResult};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Image {
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
        #[serde(default, skip_serializing)]
        images: Vec<Image>,
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
        self.push_user_with_images(content, Vec::new());
    }

    pub fn push_user_with_images(&mut self, content: impl Into<String>, images: Vec<Image>) {
        self.messages.push(Message::User {
            content: content.into(),
            images,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_only_user_messages_omit_images_and_deserialize_legacy_data() {
        let mut context = Context::new();
        context.push_user("hello");
        assert_eq!(
            serde_json::to_value(&context.messages()[0]).unwrap(),
            json!({"User": {"content": "hello"}})
        );

        let message: Message =
            serde_json::from_value(json!({"User": {"content": "legacy"}})).unwrap();
        assert!(matches!(message, Message::User { images, .. } if images.is_empty()));
    }

    #[test]
    fn push_user_with_images_preserves_image_data() {
        let image = Image {
            media_type: "image/png".into(),
            data: "aGVsbG8=".into(),
        };
        let mut context = Context::new();
        context.push_user_with_images("look", vec![image.clone()]);

        assert!(matches!(
            &context.messages()[0],
            Message::User { content, images }
                if content == "look" && images == std::slice::from_ref(&image)
        ));
    }
}
