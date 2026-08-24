//! Composer input and atomic held entities. Large text pastes and clipboard
//! images share one marker mechanism for deletion and navigation.

use base64::Engine;
use orca_harness_core::Image;

use super::format::byte_index;
use super::state::HeldInput;
use super::{push_error, push_notice, App};

pub(super) const PASTE_INLINE_MAX: usize = 200;

pub(super) fn insert_at_cursor(app: &mut App, text: &str) {
    let at = byte_index(&app.composer, app.cursor);
    app.composer.insert_str(at, text);
    app.cursor += text.chars().count();
}

pub(super) fn insert_paste(app: &mut App, text: &str) {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    if !text.contains('\n') && text.chars().count() <= PASTE_INLINE_MAX {
        insert_at_cursor(app, &text);
        return;
    }
    app.pastes.push(HeldInput::Text(text));
    let marker = app
        .pastes
        .last()
        .expect("just pushed")
        .marker(app.pastes.len());
    insert_at_cursor(app, &marker);
}

pub(super) fn insert_clipboard_image(app: &mut App) {
    match super::clipboard_image::read() {
        Ok(Some(bytes)) => insert_image_bytes(app, bytes),
        Ok(None) => push_notice(app, "clipboard does not contain an image"),
        Err(error) => push_error(app, error),
    }
}

fn insert_image_bytes(app: &mut App, bytes: Vec<u8>) {
    let label = unique_clipboard_label(&app.pastes);
    let image = Image {
        media_type: "image/png".into(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    };
    app.pastes.push(HeldInput::Image { label, image });
    let marker = app
        .pastes
        .last()
        .expect("just pushed")
        .marker(app.pastes.len());
    insert_at_cursor(app, &marker);
}

fn unique_clipboard_label(pastes: &[HeldInput]) -> String {
    let count = pastes
        .iter()
        .filter(|held| {
            matches!(held, HeldInput::Image { label, .. } if label == "image.png" || label.starts_with("image.png · "))
        })
        .count();
    if count == 0 {
        "image.png".into()
    } else {
        format!("image.png · {}", count + 1)
    }
}

pub(super) fn marker_spans(pastes: &[HeldInput], text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for (i, held) in pastes.iter().enumerate() {
        let marker = held.marker(i + 1);
        let width = marker.chars().count();
        let mut from = 0;
        while let Some(offset) = text[from..].find(&marker) {
            let start_byte = from + offset;
            let start = text[..start_byte].chars().count();
            spans.push((start, start + width));
            from = start_byte + marker.len();
        }
    }
    spans.sort_unstable();
    spans
}

pub(super) fn marker_ending_at(
    pastes: &[HeldInput],
    text: &str,
    cursor: usize,
) -> Option<(usize, usize)> {
    marker_spans(pastes, text)
        .into_iter()
        .find(|(_, end)| *end == cursor)
}

pub(super) fn marker_starting_at(
    pastes: &[HeldInput],
    text: &str,
    cursor: usize,
) -> Option<(usize, usize)> {
    marker_spans(pastes, text)
        .into_iter()
        .find(|(start, _)| *start == cursor)
}

pub(super) fn remove_marker(app: &mut App, start: usize, end: usize) {
    let from = byte_index(&app.composer, start);
    let to = byte_index(&app.composer, end);
    app.composer.replace_range(from..to, "");
    app.cursor = start;
}

pub(super) fn expand_pastes(pastes: &[HeldInput], text: &str) -> String {
    let mut out = text.to_string();
    for (i, held) in pastes.iter().enumerate() {
        if let HeldInput::Text(value) = held {
            out = out.replace(&held.marker(i + 1), value);
        }
    }
    out
}

pub(super) fn prompt_images(pastes: &[HeldInput], text: &str) -> Vec<Image> {
    let mut found = Vec::new();
    for (i, held) in pastes.iter().enumerate() {
        let HeldInput::Image { image, .. } = held else {
            continue;
        };
        let marker = held.marker(i + 1);
        for (byte, _) in text.match_indices(&marker) {
            found.push((byte, image.clone()));
        }
    }
    found.sort_by_key(|(byte, _)| *byte);
    found.into_iter().map(|(_, image)| image).collect()
}

pub(super) fn image_marker_spans(pastes: &[HeldInput], text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for (i, held) in pastes.iter().enumerate() {
        if !matches!(held, HeldInput::Image { .. }) {
            continue;
        }
        let marker = held.marker(i + 1);
        let width = marker.chars().count();
        for (byte, _) in text.match_indices(&marker) {
            let start = text[..byte].chars().count();
            spans.push((start, start + width));
        }
    }
    spans
}

#[cfg(test)]
pub(super) fn insert_image_bytes_for_test(app: &mut App, bytes: Vec<u8>) {
    insert_image_bytes(app, bytes);
}
