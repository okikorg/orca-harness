//! Thinking and Work rails with one shared header vocabulary.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::section::Section;

#[derive(Clone, Copy)]
pub enum ActivityRailKind {
    Thinking,
    Work,
}

impl ActivityRailKind {
    fn label(self) -> &'static str {
        match self {
            Self::Thinking => "Thinking",
            Self::Work => "Work",
        }
    }
}

pub struct ActivityRail {
    rail: Section,
}

impl ActivityRail {
    pub fn new(
        kind: ActivityRailKind,
        marker: &str,
        summary: impl Into<String>,
        style: Style,
    ) -> Self {
        let header = format!("  {marker} {} · {}", kind.label(), summary.into());
        Self {
            rail: Section::rail(Line::from(Span::styled(header, style))),
        }
    }

    pub fn push(&mut self, row: Line<'static>) {
        self.rail.push(row);
    }

    pub fn extend<I>(&mut self, rows: I)
    where
        I: IntoIterator<Item = Line<'static>>,
    {
        self.rail.extend(rows);
    }

    pub fn append_to(self, transcript: &mut Vec<Line<'static>>) {
        self.rail.append_to(transcript);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_share_the_same_header_shape() {
        let mut lines = Vec::new();
        ActivityRail::new(
            ActivityRailKind::Thinking,
            "•",
            "1s · 1 update",
            Style::default(),
        )
        .append_to(&mut lines);
        ActivityRail::new(ActivityRailKind::Work, "•", "1 tool", Style::default())
            .append_to(&mut lines);

        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(
            text,
            ["  • Thinking · 1s · 1 update", "", "  • Work · 1 tool"]
        );
    }
}
