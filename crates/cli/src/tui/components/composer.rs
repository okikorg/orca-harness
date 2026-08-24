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
    pills: &'a [(usize, usize)],
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
        pills: &'a [(usize, usize)],
    ) -> Self {
        Self {
            text,
            cursor,
            placeholder,
            width,
            accent,
            dim,
            pills,
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
        let visible_chars: Vec<char> = chars
            .iter()
            .skip(start)
            .take(inner_width)
            .copied()
            .collect();
        let line = if self.text.is_empty() {
            Line::from(vec![
                Span::styled("│ ", self.accent),
                Span::styled(self.placeholder.to_string(), self.dim),
            ])
        } else {
            let mut spans = vec![Span::styled("│ ", self.accent)];
            let mut at = 0;
            while at < visible_chars.len() {
                let absolute = start + at;
                let pill = self
                    .pills
                    .iter()
                    .find(|(pill_start, pill_end)| absolute >= *pill_start && absolute < *pill_end);
                let end = match pill {
                    Some((_, pill_end)) => (*pill_end).min(start + visible_chars.len()) - start,
                    None => self
                        .pills
                        .iter()
                        .filter_map(|(pill_start, _)| pill_start.checked_sub(start))
                        .filter(|pill_start| *pill_start > at)
                        .min()
                        .unwrap_or(visible_chars.len()),
                };
                let text: String = visible_chars[at..end].iter().collect();
                spans.push(match pill {
                    Some(_) => Span::styled(text, self.accent),
                    None => Span::raw(text),
                });
                at = end;
            }
            Line::from(spans)
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
    fn image_pill_uses_the_accent_style() {
        let accent = Style::default().fg(ratatui::style::Color::Cyan);
        let rendered = Composer::new(
            "see [▧ shot.png] now",
            20,
            "unused",
            40,
            accent,
            Style::default(),
            &[(4, 16)],
        )
        .render();
        assert_eq!(rendered.line.spans[2].content.as_ref(), "[▧ shot.png]");
        assert_eq!(rendered.line.spans[2].style, accent);
    }

    #[test]
    fn long_input_keeps_the_cursor_visible() {
        let rendered = Composer::new(
            "abcdefghijklmno",
            15,
            "unused",
            12,
            Style::default(),
            Style::default(),
            &[],
        )
        .render();
        assert_eq!(rendered.cursor_x, 10);
        assert_eq!(rendered.line.spans[1].content.as_ref(), "hijklmno");
    }
}
