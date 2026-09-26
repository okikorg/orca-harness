//! The one "header plus rows" container behind every rail, tray and
//! inspector block.
//!
//! Three surfaces used to carry their own copy of this shape with three
//! spacing rules. The rules still differ, because the surfaces sit in
//! different places, but they are now three constructors on one type: a
//! transcript rail keeps the shared section rhythm, an overlay tray puts one
//! blank row under its header, and an inspector block separates itself from
//! whatever came before it.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::transcript::{append_block, BlockSpacing};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Spacing {
    /// Transcript rail: `append_block` decides the gap before the header.
    Rail,
    /// Overlay tray: header, one blank row, rows.
    Tray,
    /// Inspector block: a blank row before the header when it follows
    /// another block.
    Inspector,
}

pub struct Section {
    header: Line<'static>,
    /// An inspector label drawn as a titled rule to this width.
    rule: Option<usize>,
    rows: Vec<Line<'static>>,
    spacing: Spacing,
}

impl Section {
    /// A transcript rail: one header line plus tightly packed detail rows.
    pub fn rail(header: Line<'static>) -> Self {
        Self::new(header, Spacing::Rail)
    }

    /// An overlay tray with the standard two-cell indent on its header.
    pub fn tray(header: impl Into<String>, style: Style) -> Self {
        Self::new(tray_header(header, style), Spacing::Tray)
    }

    /// An inspector block with the standard two-cell indent on its label.
    pub fn inspector(label: impl Into<String>, style: Style) -> Self {
        Self::new(indented(label, style), Spacing::Inspector)
    }

    fn new(header: Line<'static>, spacing: Spacing) -> Self {
        Self {
            header,
            rule: None,
            rows: Vec::new(),
            spacing,
        }
    }

    /// Draw the label as an uppercase title with a hairline to `width`,
    /// so a block reads as a section without adding panel chrome.
    pub fn ruled(mut self, width: usize) -> Self {
        self.rule = Some(width);
        self
    }

    pub fn push(&mut self, row: Line<'static>) {
        self.rows.push(row);
    }

    pub fn extend<I>(&mut self, rows: I)
    where
        I: IntoIterator<Item = Line<'static>>,
    {
        self.rows.extend(rows);
    }

    /// The section on its own, header first.
    pub fn lines(self) -> Vec<Line<'static>> {
        let mut lines = Vec::with_capacity(self.rows.len() + 2);
        lines.push(match self.rule {
            Some(width) => ruled_header(self.header, width),
            None => self.header,
        });
        if self.spacing == Spacing::Tray {
            lines.push(Line::from(""));
        }
        lines.extend(self.rows);
        lines
    }

    /// Append after existing content with this section's spacing rule.
    pub fn append_to(self, target: &mut Vec<Line<'static>>) {
        match self.spacing {
            Spacing::Rail => {
                append_block(target, self.lines(), BlockSpacing::Section, None);
            }
            Spacing::Inspector => {
                if !target.is_empty() {
                    target.push(Line::from(""));
                }
                target.extend(self.lines());
            }
            Spacing::Tray => target.extend(self.lines()),
        }
    }
}

fn ruled_header(header: Line<'static>, width: usize) -> Line<'static> {
    let style = header
        .spans
        .first()
        .map(|span| span.style)
        .unwrap_or_default();
    // Only the label is set in capitals; any summary after it keeps its
    // case, since it names paths, languages and units.
    let text = header.to_string();
    let title = match text.split_once(" · ") {
        Some((label, rest)) => format!("{} · {rest}", label.to_uppercase()),
        None => text.to_uppercase(),
    };
    let used = crate::view::cell_width(&title) + 1;
    let rule = "─".repeat(width.saturating_sub(used));
    super::layout::fit(
        Line::from(vec![
            Span::styled(title, style),
            Span::styled(format!(" {rule}"), style),
        ]),
        width,
    )
}

fn indented(text: impl Into<String>, style: Style) -> Line<'static> {
    Line::from(Span::styled(format!("  {}", text.into()), style))
}

/// A tray header: the indent, then the title with its keys set in the
/// shared key style.
fn tray_header(text: impl Into<String>, style: Style) -> Line<'static> {
    let mut spans = vec![Span::styled("  ", style)];
    spans.extend(super::keys::header_spans(&text.into(), style));
    Line::from(spans)
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
    fn tray_owns_header_indent_and_single_spacer() {
        let mut tray = Section::tray("Usage · esc close", Style::default());
        tray.push(Line::from("row"));
        assert_eq!(text(&tray.lines()), ["  Usage · esc close", "", "row"]);
        assert!(tray_header("Usage · esc close", Style::default())
            .spans
            .iter()
            .any(|span| span.content == "esc" && span.style == crate::view::theme().key));
    }

    #[test]
    fn rail_is_a_section_with_tight_internal_rows() {
        let mut transcript = vec![Line::from("prose")];
        let mut rail = Section::rail(Line::from("Thinking"));
        rail.push(Line::from("detail one"));
        rail.push(Line::from("detail two"));
        rail.append_to(&mut transcript);
        assert_eq!(
            text(&transcript),
            ["prose", "", "Thinking", "detail one", "detail two"]
        );
    }

    #[test]
    fn a_ruled_inspector_label_is_an_uppercase_title_with_a_rule_to_the_edge() {
        let section = Section::inspector("output", Style::default()).ruled(20);
        let lines = section.lines();
        assert_eq!(text(&lines), ["  OUTPUT ───────────"]);
        assert_eq!(lines[0].width(), 20);
    }

    #[test]
    fn inspector_blocks_separate_from_the_previous_block_only() {
        let mut target = Vec::new();
        let mut first = Section::inspector("input", Style::default());
        first.push(Line::from("a"));
        first.append_to(&mut target);
        let mut second = Section::inspector("output", Style::default());
        second.push(Line::from("b"));
        second.append_to(&mut target);
        assert_eq!(text(&target), ["  input", "a", "", "  output", "b"]);
    }
}
