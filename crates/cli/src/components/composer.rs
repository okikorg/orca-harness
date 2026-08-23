//! Single-line composer with a horizontally scrolling text window.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

pub struct Composer<'a> {
    text: &'a str,
    cursor: usize,
    placeholder: &'a str,
    width: usize,
    accent: Style,
    dim: Style,
}

pub struct ComposerRender {
    pub line: Line<'static>,
    /// Cursor column relative to the component's left edge.
    pub cursor_x: u16,
}

impl<'a> Composer<'a> {
    pub fn new(
        text: &'a str,
        cursor: usize,
        placeholder: &'a str,
        width: usize,
        accent: Style,
        dim: Style,
    ) -> Self {
        Self {
            text,
            cursor,
            placeholder,
            width,
            accent,
            dim,
        }
    }

    pub fn render(self) -> ComposerRender {
        let inner_width = self.width.saturating_sub(3).max(8);
        let chars: Vec<char> = self.text.chars().collect();
        let cursor = self.cursor.min(chars.len());
        let start = if cursor >= inner_width {
            cursor + 1 - inner_width
        } else {
            0
        };
        let visible: String = chars.iter().skip(start).take(inner_width).collect();
        let line = if self.text.is_empty() {
            Line::from(vec![
                Span::styled("│ ", self.accent),
                Span::styled(self.placeholder.to_string(), self.dim),
            ])
        } else {
            Line::from(vec![Span::styled("│ ", self.accent), Span::raw(visible)])
        };
        ComposerRender {
            line,
            cursor_x: 2 + (cursor - start) as u16,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_input_keeps_the_cursor_visible() {
        let rendered = Composer::new(
            "abcdefghijklmno",
            15,
            "unused",
            12,
            Style::default(),
            Style::default(),
        )
        .render();
        assert_eq!(rendered.cursor_x, 10);
        assert_eq!(rendered.line.spans[1].content.as_ref(), "hijklmno");
    }
}
