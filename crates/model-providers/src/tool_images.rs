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
    /// [`MAX_TOOL_IMAGES`]. Each kept image comes with its 1-based number
    /// in the result, which its `[image N]` text marker names.
    pub(crate) fn fresh<'a>(&mut self, images: Vec<ToolImage<'a>>) -> Vec<(usize, ToolImage<'a>)> {
        let skip = self.stale.min(images.len());
        self.stale -= skip;
        images
            .into_iter()
            .enumerate()
            .skip(skip)
            .map(|(i, image)| (i + 1, image))
            .collect()
    }
}

/// The text sent beside a result's images. A lone `content` string is
/// unwrapped so its `[image N]` markers read as plain text.
pub(crate) fn text_of(output: &Value) -> Cow<'_, str> {
    match output {
        Value::String(text) => Cow::Borrowed(text),
        Value::Object(object) if object.len() == 1 => match object.get("content") {
            Some(Value::String(text)) => Cow::Borrowed(text),
            _ => Cow::Owned(output.to_string()),
        },
        _ => Cow::Owned(output.to_string()),
    }
}

/// One piece of a tool result, in the order the tool returned it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Part<'a> {
    Text(String),
    Image(ToolImage<'a>),
}

/// Split `text` after each image's `[image N]` marker and put the image
/// there, so text and images keep their original order. Images whose
/// marker is missing follow the text. Blank text pieces are dropped.
pub(crate) fn interleave<'a>(text: &str, images: Vec<(usize, ToolImage<'a>)>) -> Vec<Part<'a>> {
    let mut parts = Vec::new();
    let mut unplaced = Vec::new();
    let mut rest = text;
    let push_text = |parts: &mut Vec<Part<'a>>, text: &str| {
        if !text.trim().is_empty() {
            parts.push(Part::Text(text.to_string()));
        }
    };
    for (number, image) in images {
        let marker = format!("[image {number}]");
        match rest.find(&marker) {
            Some(at) => {
                let end = at + marker.len();
                push_text(&mut parts, &rest[..end]);
                parts.push(Part::Image(image));
                rest = &rest[end..];
            }
            None => unplaced.push(image),
        }
    }
    push_text(&mut parts, rest);
    parts.extend(unplaced.into_iter().map(Part::Image));
    parts
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
    fn interleave_places_each_image_after_its_marker() {
        let png = ToolImage {
            media_type: "image/png",
            data: "A",
        };
        let jpg = ToolImage {
            media_type: "image/jpeg",
            data: "B",
        };
        let text = "before\n[image 1]\nbetween\n[image 2]";
        assert_eq!(
            interleave(text, vec![(1, png), (2, jpg)]),
            vec![
                Part::Text("before\n[image 1]".into()),
                Part::Image(png),
                Part::Text("\nbetween\n[image 2]".into()),
                Part::Image(jpg),
            ]
        );
        // Image 1 fell out of the request window: its marker stays as text.
        assert_eq!(
            interleave(text, vec![(2, jpg)]),
            vec![Part::Text(text.into()), Part::Image(jpg)]
        );
        // No markers (e.g. structured output): images follow the text.
        assert_eq!(
            interleave("{\"width\":10}", vec![(1, png)]),
            vec![Part::Text("{\"width\":10}".into()), Part::Image(png)]
        );
    }

    #[test]
    fn text_of_unwraps_only_a_lone_content_string() {
        assert_eq!(text_of(&json!({"content": "a [image 1]"})), "a [image 1]");
        assert_eq!(text_of(&json!("plain")), "plain");
        assert_eq!(
            text_of(&json!({"content": "a", "x": 1})),
            "{\"content\":\"a\",\"x\":1}"
        );
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
