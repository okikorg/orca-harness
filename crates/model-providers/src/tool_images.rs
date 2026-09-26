//! Images a tool returns next to its output. The kernel's tool output is
//! plain JSON, so images ride in it under [`TOOL_IMAGES_KEY`] as
//! `[{"media_type", "data"}]` (base64). Encoders send them as image
//! blocks tied to their call and the rest of the output as text.

use std::borrow::Cow;

use orca_harness_core::Context;
use orca_harness_core::Message;
use serde_json::Value;

/// Output key holding a tool's images.
pub const TOOL_IMAGES_KEY: &str = "_images";

/// How many tool-result images a request carries: the most recent ones.
/// Providers cap images per request, and a long browser run would
/// otherwise resend every screenshot it ever took on each turn.
pub const MAX_TOOL_IMAGES: usize = 20;

/// One image borrowed from a tool output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolImage<'a> {
    pub media_type: &'a str,
    pub data: &'a str,
}

impl ToolImage<'_> {
    pub(crate) fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data)
    }
}

/// Split a tool output into its text part (without the images key) and
/// its images. An output without a well-formed image list is all text.
pub fn split(output: &Value) -> (Cow<'_, Value>, Vec<ToolImage<'_>>) {
    let Some(images) = images(output) else {
        return (Cow::Borrowed(output), Vec::new());
    };
    let mut rest = output.as_object().cloned().unwrap_or_default();
    rest.remove(TOOL_IMAGES_KEY);
    (Cow::Owned(Value::Object(rest)), images)
}

fn images(output: &Value) -> Option<Vec<ToolImage<'_>>> {
    output
        .get(TOOL_IMAGES_KEY)?
        .as_array()?
        .iter()
        .map(|image| {
            Some(ToolImage {
                media_type: image.get("media_type")?.as_str()?,
                data: image.get("data")?.as_str()?,
            })
        })
        .collect()
}

/// Picks the tool-result images one request sends: build it per request,
/// then call [`fresh`](Self::fresh) for each result in message order.
pub(crate) struct Recent {
    stale: usize,
}

impl Recent {
    pub(crate) fn new(context: &Context) -> Self {
        let total: usize = context
            .messages()
            .iter()
            .filter_map(|message| match message {
                Message::Tool { results } => Some(results),
                _ => None,
            })
            .flatten()
            .filter_map(|result| images(&result.output))
            .map(|images| images.len())
            .sum();
        Self {
            stale: total.saturating_sub(MAX_TOOL_IMAGES),
        }
    }

    /// Drop this result's images that are older than the most recent
    /// [`MAX_TOOL_IMAGES`].
    pub(crate) fn fresh<'a>(&mut self, mut images: Vec<ToolImage<'a>>) -> Vec<ToolImage<'a>> {
        let skip = self.stale.min(images.len());
        self.stale -= skip;
        images.drain(..skip);
        images
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_harness_core::{ToolCall, ToolResult};
    use serde_json::json;

    #[test]
    fn split_separates_images_and_leaves_other_outputs_alone() {
        let output = json!({"content": "[image 1]", "_images": [{"media_type": "image/png", "data": "iVBO"}]});
        let (text, images) = split(&output);
        assert_eq!(*text, json!({"content": "[image 1]"}));
        assert_eq!(
            images,
            vec![ToolImage {
                media_type: "image/png",
                data: "iVBO"
            }]
        );

        // A malformed list is ordinary output, sent as text.
        let odd = json!({"_images": [{"url": "x"}]});
        let (text, images) = split(&odd);
        assert_eq!(*text, odd);
        assert!(images.is_empty());
    }

    #[test]
    fn recent_keeps_only_the_newest_images() {
        let call = ToolCall {
            id: "c".into(),
            name: "shot".into(),
            arguments: json!({}),
        };
        let mut context = Context::new();
        for step in 0..MAX_TOOL_IMAGES + 2 {
            context.append_tool_results(vec![ToolResult::ok(
                &call,
                json!({"_images": [{"media_type": "image/png", "data": step.to_string()}]}),
            )]);
        }
        let mut recent = Recent::new(&context);
        let sent: Vec<usize> = context
            .messages()
            .iter()
            .map(|message| match message {
                Message::Tool { results } => recent.fresh(split(&results[0].output).1).len(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(sent[..3], [0, 0, 1]);
        assert_eq!(sent.iter().sum::<usize>(), MAX_TOOL_IMAGES);
    }
}
