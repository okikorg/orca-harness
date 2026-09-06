//! Width-safe presentation helpers for component-owned lines.
use ratatui::text::Line;

pub(super) fn fit(line: Line<'static>, width: usize) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    Line::from(crate::view::truncate_styled_line(line.spans, width))
}

pub(super) fn fit_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    lines.into_iter().map(|line| fit(line, width)).collect()
}

/// Wrap meaningful text without losing its ending on narrow panes.
pub(super) fn wrapped(
    text: &str,
    prefix: &str,
    width: usize,
    style: ratatui::style::Style,
) -> Vec<Line<'static>> {
    let indent = " ".repeat(crate::view::cell_width(prefix));
    let body = width.saturating_sub(crate::view::cell_width(prefix)).max(1);
    textwrap::wrap(&crate::view::sanitize_cells(text), body)
        .into_iter()
        .enumerate()
        .map(|(index, part)| {
            fit(
                Line::from(ratatui::text::Span::styled(
                    format!("{}{}", if index == 0 { prefix } else { &indent }, part),
                    style,
                )),
                width,
            )
        })
        .collect()
}
