//! Composer input: applying typed text and bracketed pastes to the
//! composer, and the paste-marker machinery that stands a large paste in
//! for a short marker the user can edit around. Pastes are held aside in
//! `App::pastes` (indexed by marker number) until the prompt is sent.

use super::format::byte_index;
use super::App;

/// Pasting more than this many characters inline is held as a marker.
pub(super) const PASTE_INLINE_MAX: usize = 200;

/// The composer stand-in for held paste `index` (1-based).
pub(super) fn paste_marker(index: usize, lines: usize) -> String {
    let unit = if lines == 1 { "line" } else { "lines" };
    format!("[Pasted text #{index}, {lines} {unit}]")
}

/// Line count as the marker reports it: what the user sees pasted, so a
/// trailing newline is not a line of its own.
pub(super) fn paste_line_count(text: &str) -> usize {
    text.lines().count().max(1)
}

pub(super) fn insert_at_cursor(app: &mut App, text: &str) {
    let at = byte_index(&app.composer, app.cursor);
    app.composer.insert_str(at, text);
    app.cursor += text.chars().count();
}

/// Put a bracketed paste into the composer: short single-line text as if
/// typed, anything larger as a marker standing for the held original.
pub(super) fn insert_paste(app: &mut App, text: &str) {
    // Terminals forward whatever line endings the source had; normalize
    // so a CRLF paste does not count double or leave stray carriage
    // returns in the prompt.
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    if !text.contains('\n') && text.chars().count() <= PASTE_INLINE_MAX {
        insert_at_cursor(app, &text);
        return;
    }
    app.pastes.push(text);
    let index = app.pastes.len();
    let lines = paste_line_count(&app.pastes[index - 1]);
    let marker = paste_marker(index, lines);
    insert_at_cursor(app, &marker);
}

/// Char spans of every paste marker present in `text`. Markers are found
/// as whole literals, so the composer can treat each as one unit — one
/// backspace, one arrow step — rather than 26 characters the user has to
/// chew through and can corrupt halfway.
pub(super) fn marker_spans(pastes: &[String], text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    if pastes.is_empty() || !text.contains("[Pasted text #") {
        return spans;
    }
    for (i, paste) in pastes.iter().enumerate() {
        let marker = paste_marker(i + 1, paste_line_count(paste));
        let width = marker.chars().count();
        let mut from = 0;
        while let Some(offset) = text[from..].find(&marker) {
            let start_byte = from + offset;
            let start = text[..start_byte].chars().count();
            spans.push((start, start + width));
            from = start_byte + marker.len();
        }
    }
    spans
}

/// The marker ending exactly at `cursor`, if the cursor sits against one.
pub(super) fn marker_ending_at(
    pastes: &[String],
    text: &str,
    cursor: usize,
) -> Option<(usize, usize)> {
    marker_spans(pastes, text)
        .into_iter()
        .find(|(_, end)| *end == cursor)
}

/// The marker starting exactly at `cursor`.
pub(super) fn marker_starting_at(
    pastes: &[String],
    text: &str,
    cursor: usize,
) -> Option<(usize, usize)> {
    marker_spans(pastes, text)
        .into_iter()
        .find(|(start, _)| *start == cursor)
}

/// Cut the marker span `(start, end)` out of the composer whole.
pub(super) fn remove_marker(app: &mut App, start: usize, end: usize) {
    let from = byte_index(&app.composer, start);
    let to = byte_index(&app.composer, end);
    app.composer.replace_range(from..to, "");
    app.cursor = start;
}

/// Restore held pastes into `text`, marker by marker. Markers are matched
/// as whole literals rather than parsed, so text that merely looks like
/// one is left alone.
pub(super) fn expand_pastes(pastes: &[String], text: &str) -> String {
    if pastes.is_empty() || !text.contains("[Pasted text #") {
        return text.to_string();
    }
    let mut out = text.to_string();
    for (i, paste) in pastes.iter().enumerate() {
        let marker = paste_marker(i + 1, paste_line_count(paste));
        if out.contains(&marker) {
            out = out.replace(&marker, paste);
        }
    }
    out
}
