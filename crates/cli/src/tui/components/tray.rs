//! Shared frame for read-only and input overlay trays.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

pub struct Tray {
    header: String,
    header_style: Style,
    rows: Vec<Line<'static>>,
}

impl Tray {
    pub fn new(header: impl Into<String>, header_style: Style) -> Self {
        Self {
            header: header.into(),
            header_style,
            rows: Vec::new(),
        }
    }

    pub fn push(&mut self, row: Line<'static>) {
        self.rows.push(row);
    }

    pub fn lines(self) -> Vec<Line<'static>> {
        let mut lines = Vec::with_capacity(self.rows.len() + 2);
        lines.push(Line::from(Span::styled(
            format!("  {}", self.header),
            self.header_style,
        )));
        lines.push(Line::from(""));
        lines.extend(self.rows);
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_owns_header_indent_and_single_spacer() {
        let mut tray = Tray::new("Usage · esc close", Style::default());
        tray.push(Line::from("row"));
        let lines = tray.lines();
        assert_eq!(lines[0].spans[0].content.as_ref(), "  Usage · esc close");
        assert!(lines[1].spans.is_empty());
        assert_eq!(lines[2].spans[0].content.as_ref(), "row");
    }
}
