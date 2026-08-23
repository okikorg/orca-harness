//! Small line/text helpers shared by the transcript builders, the key
//! handlers, and the event loop: extracting plain text from a styled
//! ratatui line, wrapping a paragraph into styled lines, and splicing a
//! block into a transcript replacing a matched run of lines.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// Flatten a styled line to its plain text (span contents concatenated).
pub(crate) fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Drop the `·…○` connector "leaf" spans that join a rail to its last
/// row once that row has been pulled out of the transcript. Purely
/// cosmetic: keeps the fold markers from pointing at nothing.
pub(crate) fn clear_tool_connectors(lines: &mut [Line<'static>]) {
    for line in lines {
        line.spans.retain(|span| {
            let text = span.content.trim();
            !(text.contains('·')
                && text.ends_with('○')
                && text.chars().all(|ch| ch == '·' || ch == '○'))
        });
    }
}

/// Wrap `text` into `lines`, indenting every wrapped row with `indent`
/// and styling it with `style`. Blank paragraphs become blank lines.
pub(crate) fn push_wrapped_lines(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    indent: &str,
    style: Style,
    width: usize,
) {
    let body_width = width.saturating_sub(indent.len()).max(16);
    for paragraph in text.split('\n') {
        if paragraph.trim().is_empty() {
            lines.push(Line::from(""));
            continue;
        }
        for piece in textwrap::wrap(paragraph, body_width) {
            lines.push(Line::from(Span::styled(format!("{indent}{piece}"), style)));
        }
    }
}

/// Replace the last run of `targets` in `lines` with `replacement`.
/// `targets` are the verbatim plain-text rows (collapsed work summaries);
/// matching is by [`line_text`] so indentation/colors don't matter.
/// Returns whether a replacement happened.
pub(crate) fn replace_block(
    lines: &mut Vec<Line<'static>>,
    targets: &[String],
    replacement: &[Line<'static>],
) -> bool {
    if targets.is_empty() || targets.len() > lines.len() {
        return false;
    }
    let Some(index) = (0..=lines.len() - targets.len()).rev().find(|start| {
        lines[*start..*start + targets.len()]
            .iter()
            .map(line_text)
            .eq(targets.iter().cloned())
    }) else {
        return false;
    };
    lines.splice(index..index + targets.len(), replacement.iter().cloned());
    true
}
