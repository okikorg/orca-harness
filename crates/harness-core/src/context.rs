//! Context is model-visible state, not durable memory. Durable memory is
//! an Extension concern.

use serde::{Deserialize, Serialize};

use crate::tool::{ToolCall, ToolResult};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Image {
    pub media_type: String,
    pub data: String,
    /// Provider-fetched image URL. Absent in legacy base64 snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
}

impl Image {
    pub fn base64(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            media_type: media_type.into(),
            data: data.into(),
            source_url: None,
        }
    }

    /// An image the provider fetches. Only `http` and `https` URLs with a
    /// host are accepted; `data:`, `file:` and other schemes return `None`.
    /// The caller validates the URL against its input policy. No fetch occurs here.
    ///
    /// The URL is sent on every request that carries its message and kept
    /// in persisted sessions, so it must stay fetchable for the session's
    /// life: an expiring presigned link fails every later turn once it
    /// lapses. For the same reason a URL carrying credentials
    /// (`user:pass@`) is refused, as is one with whitespace, control or
    /// non-ASCII characters, or an invalid host or port.
    pub fn url(url: impl Into<String>) -> Option<Self> {
        let url = url.into();
        let (scheme, rest) = url.split_once("://")?;
        let web = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let printable = url.bytes().all(|byte| byte.is_ascii_graphic());
        (web && printable && valid_authority(authority)).then(|| Self {
            media_type: String::new(),
            data: String::new(),
            source_url: Some(url),
        })
    }
}

/// `host[:port]` or `[ipv6][:port]`, with no userinfo.
fn valid_authority(authority: &str) -> bool {
    // `port` keeps its leading `:`, so an empty one means no port at all.
    let (host_ok, port) = match authority.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((ip, port)) => (
                !ip.is_empty()
                    && ip
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() || b".:".contains(&b)),
                port,
            ),
            None => return false,
        },
        None => {
            let (host, port) = authority.split_at(authority.find(':').unwrap_or(authority.len()));
            let named = host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b));
            (!host.is_empty() && named, port)
        }
    };
    let port_ok = match port.strip_prefix(':') {
        Some(digits) => {
            digits.bytes().all(|b| b.is_ascii_digit())
                && digits.parse::<u16>().is_ok_and(|port| port > 0)
        }
        None => port.is_empty(),
    };
    host_ok && port_ok
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
            source_url: None,
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
    #[test]
    fn image_urls_and_legacy_base64_snapshots_round_trip() {
        let legacy = json!({"media_type":"image/png","data":"cGl4ZWw="});
        let image: Image = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(image, Image::base64("image/png", "cGl4ZWw="));
        assert_eq!(serde_json::to_value(image).unwrap(), legacy);
        let image = Image::url("https://images.example.test/pixel.png?version=2").unwrap();
        let saved = serde_json::to_value(&image).unwrap();
        assert_eq!(
            saved["source_url"],
            "https://images.example.test/pixel.png?version=2"
        );
        assert_eq!(serde_json::from_value::<Image>(saved).unwrap(), image);
    }

    #[test]
    fn image_urls_must_be_http_or_https() {
        for url in [
            "https://images.example.test/pixel.png",
            "http://images.example.test/pixel.png",
            "HTTPS://images.example.test/pixel.png",
            "https://images.example.test:8443/a%20b.png?sig=x#frag",
            "http://[::1]:8080/pixel.png",
            "https://cdn_1.example.test",
        ] {
            assert_eq!(Image::url(url).unwrap().source_url.as_deref(), Some(url));
        }
        for url in [
            "data:image/png;base64,cGl4ZWw=",
            "file:///etc/passwd",
            "ftp://images.example.test/pixel.png",
            "javascript:alert(1)",
            "//images.example.test/pixel.png",
            "https://",
            "https:///pixel.png",
            "",
            "https://user:pass@images.example.test/pixel.png",
            "https://token@images.example.test/pixel.png",
            "https://@",
            "https:// /pixel.png",
            "https://images.example.test/a b.png",
            "https://images.example.test\n",
            " https://images.example.test",
            "https://\\evil.test",
            "https://images.example.test:99999/x",
            "https://images.example.test:0/x",
            "https://images.example.test:/x",
            "https://images.example.test:80:80/x",
            "https://[::1/x",
            "https://[]/x",
            "https://bücher.example/x",
        ] {
            assert!(Image::url(url).is_none(), "{url}");
        }
    }
}
