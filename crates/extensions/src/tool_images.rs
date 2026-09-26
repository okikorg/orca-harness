//! Tool images in extension hooks. Images ride in a tool's output under
//! [`TOOL_IMAGES_KEY`] as base64; text handling (truncation, the context
//! estimate, events) must not treat that data as text.

use serde_json::{json, Value};

/// Output key the model providers read tool images from
/// (`orca_harness_model_providers::tool_images::TOOL_IMAGES_KEY`).
const TOOL_IMAGES_KEY: &str = "_images";

/// `(media_type, data)` of each image, or empty unless the output holds a
/// well-formed image list (the same rule the providers apply).
fn images(output: &Value) -> Vec<(&str, &str)> {
    output
        .get(TOOL_IMAGES_KEY)
        .and_then(Value::as_array)
        .and_then(|images| {
            images
                .iter()
                .map(|image| {
                    Some((
                        image.get("media_type")?.as_str()?,
                        image.get("data")?.as_str()?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Remove a well-formed image list from `output`, to put back with
/// [`restore`] after text-only processing.
pub(crate) fn take(output: &mut Value) -> Option<Value> {
    if images(output).is_empty() {
        return None;
    }
    output.as_object_mut()?.remove(TOOL_IMAGES_KEY)
}

pub(crate) fn restore(output: &mut Value, images: Option<Value>) {
    if let (Some(object), Some(images)) = (output.as_object_mut(), images) {
        object.insert(TOOL_IMAGES_KEY.into(), images);
    }
}

/// Image count and total base64 length in a tool output.
pub(crate) fn measure(output: &Value) -> (usize, usize) {
    let images = images(output);
    (
        images.len(),
        images.iter().map(|(_, data)| data.len()).sum(),
    )
}

/// The output with each image's data replaced by its decoded size, for
/// display and event streams.
pub(crate) fn summarize(output: &Value) -> Value {
    let images = images(output);
    if images.is_empty() {
        return output.clone();
    }
    let summary: Vec<Value> = images
        .iter()
        .map(|(media_type, data)| {
            json!({
                "media_type": media_type,
                "bytes": data.trim_end_matches('=').len() * 3 / 4,
            })
        })
        .collect();
    let mut output = output.clone();
    output[TOOL_IMAGES_KEY] = Value::Array(summary);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_only_act_on_well_formed_image_lists() {
        let mut output =
            json!({"content": "x", "_images": [{"media_type": "image/png", "data": "aGVsbG8="}]});
        assert_eq!(measure(&output), (1, 8));
        assert_eq!(
            summarize(&output),
            json!({"content": "x", "_images": [{"media_type": "image/png", "bytes": 5}]})
        );
        let images = take(&mut output);
        assert_eq!(output, json!({"content": "x"}));
        restore(&mut output, images);
        assert_eq!(measure(&output), (1, 8));

        let mut odd = json!({"_images": "not a list"});
        assert!(take(&mut odd).is_none());
        assert_eq!(summarize(&odd), odd);
        assert_eq!(measure(&odd), (0, 0));
    }
}
