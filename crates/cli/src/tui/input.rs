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
    if insert_pasted_image_paths(app, &text) {
        return;
    }
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

/// Terminals that intercept an image paste themselves write the image to a
/// temp file and hand us its path as ordinary bracketed paste. Those paths
/// become the same atomic pill as a Ctrl+V clipboard read; anything we
/// cannot read as an image falls through to the plain-text paste path.
fn insert_pasted_image_paths(app: &mut App, text: &str) -> bool {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return false;
    }
    let mut loaded = Vec::new();
    for line in &lines {
        match read_image_file(line) {
            Some(image) => loaded.push(image),
            None => return false,
        }
    }
    for (bytes, media_type) in loaded {
        insert_image_bytes_typed(app, bytes, media_type);
    }
    true
}

fn read_image_file(path: &str) -> Option<(Vec<u8>, &'static str)> {
    let path = path.strip_prefix("file://").unwrap_or(path);
    // Terminals quote paths that contain spaces; strip one matched pair.
    let path = path
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .or_else(|| {
            path.strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .unwrap_or(path);
    let path = match path.strip_prefix("~/") {
        Some(rest) => {
            std::env::var("HOME")
                .ok()?
                .trim_end_matches('/')
                .to_string()
                + "/"
                + rest
        }
        None => path.to_string(),
    };
    if !path.starts_with('/') {
        return None;
    }
    let media_type = image_media_type(&path)?;
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() as usize > super::clipboard_image::MAX_IMAGE_BYTES {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some((bytes, media_type))
}

fn image_media_type(path: &str) -> Option<&'static str> {
    let extension = path.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

fn insert_image_bytes(app: &mut App, bytes: Vec<u8>) {
    insert_image_bytes_typed(app, bytes, "image/png");
}

fn insert_image_bytes_typed(app: &mut App, bytes: Vec<u8>, media_type: &str) {
    let label = unique_clipboard_label(&app.pastes, media_type);
    let image = Image {
        media_type: media_type.into(),
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

fn unique_clipboard_label(pastes: &[HeldInput], media_type: &str) -> String {
    let base = format!(
        "image.{}",
        media_type.rsplit_once('/').map_or("png", |(_, sub)| sub)
    );
    let numbered = format!("{base} · ");
    let count = pastes
        .iter()
        .filter(|held| {
            matches!(held, HeldInput::Image { label, .. } if *label == base || label.starts_with(&numbered))
        })
        .count();
    if count == 0 {
        base
    } else {
        format!("{base} · {}", count + 1)
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
