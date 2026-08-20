//! Truncation extension: caps oversized tool outputs in `after_tool`
//! before they reach the context (protecting the window) or a downstream
//! stream with a hard line cap. String fields over the limit are trimmed
//! head-and-tail with an elision marker; the result stays valid JSON.

use async_trait::async_trait;
use serde_json::Value;

use orca_harness_core::{Extension, ExtensionError, Subscriptions, ToolCall, ToolResult};

pub struct Truncation {
    /// Maximum length, in characters, of any single string in the output.
    max_string_chars: usize,
}

impl Default for Truncation {
    fn default() -> Self {
        // Comfortably under a 4 MB downstream line cap while leaving room
        // for many strings in one result.
        Self {
            max_string_chars: 16 * 1024,
        }
    }
}

impl Truncation {
    pub fn new(max_string_chars: usize) -> Self {
        Self { max_string_chars }
    }

    fn truncate_value(&self, value: &mut Value) -> bool {
        match value {
            Value::String(s) => {
                let len = s.chars().count();
                if len > self.max_string_chars {
                    let keep = self.max_string_chars / 2;
                    let head: String = s.chars().take(keep).collect();
                    let tail: String = s.chars().skip(len - keep).collect();
                    let elided = len - 2 * keep;
                    *s = format!("{head}\n… [{elided} chars elided] …\n{tail}");
                    true
                } else {
                    false
                }
            }
            Value::Array(items) => {
                let mut any = false;
                for item in items {
                    any |= self.truncate_value(item);
                }
                any
            }
            Value::Object(map) => {
                let mut any = false;
                for (_, v) in map.iter_mut() {
                    any |= self.truncate_value(v);
                }
                any
            }
            _ => false,
        }
    }
}

#[async_trait]
impl Extension for Truncation {
    fn name(&self) -> &str {
        "truncation"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().after_tool()
    }

    async fn after_tool(
        &self,
        _call: &ToolCall,
        mut result: ToolResult,
    ) -> Result<ToolResult, ExtensionError> {
        let truncated = self.truncate_value(&mut result.output);
        if truncated {
            if let Value::Object(map) = &mut result.output {
                map.insert("_truncated".into(), Value::Bool(true));
            }
        }
        Ok(result)
    }
}
