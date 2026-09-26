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

/// `left` with `right` anchored at the right edge of `width`. When both
/// do not fit with a cell between them, `right` gives way: it is always
/// the recoverable half (a hint, a duration, a count).
pub(super) fn right_align(
    left: Vec<ratatui::text::Span<'static>>,
    right: Vec<ratatui::text::Span<'static>>,
    width: usize,
) -> Line<'static> {
    let used = crate::view::spans_width(&left);
    let tail = crate::view::spans_width(&right);
    if right.is_empty() || used + 1 + tail > width {
        return fit(Line::from(left), width);
    }
    let mut spans = left;
    spans.push(ratatui::text::Span::raw(" ".repeat(width - used - tail)));
    spans.extend(right);
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    #[test]
    fn right_align_anchors_the_tail_and_drops_it_when_tight() {
        let line = right_align(vec![Span::raw("left")], vec![Span::raw("2ms")], 12);
        assert_eq!(line.to_string(), "left     2ms");
        let tight = right_align(vec![Span::raw("left")], vec![Span::raw("2ms")], 7);
        assert_eq!(tight.to_string(), "left");
    }
}
