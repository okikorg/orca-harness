//! Shared transcript component composition.
//!
//! Every transcript surface supplies its own styled lines, while this module
//! owns edge trimming and the single-row rhythm between components. This keeps
//! prose, Thinking, Work, notices, and future transcript components from each
//! hand-inserting spacer rows.

use ratatui::text::Line;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptSpacing {
    Compact,
    Comfortable,
}

impl TranscriptSpacing {
    pub const ALL: [Self; 2] = [Self::Compact, Self::Comfortable];

    pub fn label(self) -> &'static str {
        match self {
            Self::Compact => "Compact",
            Self::Comfortable => "Comfortable",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Comfortable => "comfortable",
        }
    }

    pub fn from_slug(value: &str) -> Option<Self> {
        match value {
            "compact" => Some(Self::Compact),
            "comfortable" => Some(Self::Comfortable),
            _ => None,
        }
    }
}

static SPACING: AtomicU8 = AtomicU8::new(1);

pub fn set_transcript_spacing(spacing: TranscriptSpacing) {
    SPACING.store(spacing as u8, Ordering::Relaxed);
}

pub fn transcript_spacing() -> TranscriptSpacing {
    match SPACING.load(Ordering::Relaxed) {
        0 => TranscriptSpacing::Compact,
        _ => TranscriptSpacing::Comfortable,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BlockSpacing {
    Tight,
    Section,
}

pub fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|span| span.content.trim().is_empty())
}

fn trim_blank_edges(lines: &mut Vec<Line<'static>>) {
    let start = lines.iter().position(|line| !line_is_blank(line));
    let Some(start) = start else {
        lines.clear();
        return;
    };
    let end = lines
        .iter()
        .rposition(|line| !line_is_blank(line))
        .expect("non-empty component")
        + 1;
    lines.drain(end..);
    lines.drain(..start);
}

/// Append a rendered component with normalized edges and shared spacing.
pub fn append_block(
    target: &mut Vec<Line<'static>>,
    mut block: Vec<Line<'static>>,
    spacing: BlockSpacing,
    fallback_prior: Option<bool>,
) -> bool {
    trim_blank_edges(&mut block);
    if block.is_empty() {
        return false;
    }
    while target.last().is_some_and(line_is_blank) {
        target.pop();
    }
    let prior = target.last().map(line_is_blank).or(fallback_prior);
    if prior == Some(false)
        && spacing == BlockSpacing::Section
        && transcript_spacing() == TranscriptSpacing::Comfortable
    {
        target.push(Line::from(""));
    }
    target.extend(block);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn sections_have_exactly_one_blank_row_between_them() {
        let mut lines = vec![Line::from("prose"), Line::from("")];
        append_block(
            &mut lines,
            vec![Line::from(""), Line::from("thinking")],
            BlockSpacing::Section,
            None,
        );
        append_block(
            &mut lines,
            vec![Line::from("work")],
            BlockSpacing::Section,
            None,
        );

        assert_eq!(text(&lines), ["prose", "", "thinking", "", "work"]);
    }

    #[test]
    fn tight_components_do_not_gain_padding() {
        let mut lines = vec![Line::from("header")];
        append_block(
            &mut lines,
            vec![Line::from("detail")],
            BlockSpacing::Tight,
            None,
        );

        assert_eq!(text(&lines), ["header", "detail"]);
    }
}
