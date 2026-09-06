//! The composer: a spine, the prompt text wrapped to the width, image
//! pills, and the cursor. Long input grows the composer a row at a time
//! up to [`COMPOSER_MAX_ROWS`]; past that the rows scroll to keep the
//! cursor in view.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::view;

/// Rows the composer may occupy before it scrolls.
pub const COMPOSER_MAX_ROWS: usize = 6;
const SPINE: &str = "│ ";

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
    /// One line per visible row; never empty.
    pub lines: Vec<Line<'static>>,
    /// Cursor column relative to the component's left edge.
    pub cursor_x: u16,
    /// Cursor row within `lines`.
    pub cursor_y: u16,
}

/// One character placed in the wrapped grid.
struct Cell {
    ch: char,
    row: usize,
    /// Char index into the composer text.
    index: usize,
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

    fn inner_width(&self) -> usize {
        self.width
            .saturating_sub(view::cell_width(SPINE) + 1)
            .max(1)
    }

    /// Place every char on a row and column; the cursor sits after the
    /// char at `cursor - 1`, or wraps to the next row when that char ends
    /// a full row.
    fn layout(&self) -> (Vec<Cell>, usize, usize) {
        let inner = self.inner_width();
        let mut cells = Vec::new();
        let (mut row, mut col) = (0, 0);
        let cursor = self.cursor.min(self.text.chars().count());
        let (mut cursor_row, mut cursor_col) = (0, 0);
        for (index, ch) in self.text.chars().enumerate() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if col + w > inner {
                row += 1;
                col = 0;
            }
            if index == cursor {
                (cursor_row, cursor_col) = (row, col);
            }
            cells.push(Cell { ch, row, index });
            col += w;
        }
        if cursor == cells.len() {
            if col >= inner {
                (cursor_row, cursor_col) = (row + 1, 0);
            } else {
                (cursor_row, cursor_col) = (row, col);
            }
        }
        (cells, cursor_row, cursor_col)
    }

    pub fn render(self) -> ComposerRender {
        if self.text.is_empty() {
            return ComposerRender {
                lines: super::layout::fit_lines(
                    vec![Line::from(vec![
                        Span::styled(SPINE, self.accent),
                        Span::styled(self.placeholder.to_string(), self.dim),
                    ])],
                    self.width,
                ),
                cursor_x: view::cell_width(SPINE).min(self.width.saturating_sub(1)) as u16,
                cursor_y: 0,
            };
        }
        let (cells, cursor_row, cursor_col) = self.layout();
        let rows = cells.last().map_or(0, |cell| cell.row).max(cursor_row) + 1;
        // Scroll so the cursor row is visible; prefer showing the tail.
        let first = rows
            .saturating_sub(COMPOSER_MAX_ROWS)
            .min(cursor_row)
            .max(cursor_row.saturating_sub(COMPOSER_MAX_ROWS - 1));
        let last = (first + COMPOSER_MAX_ROWS).min(rows);

        let mut lines = Vec::with_capacity(last - first);
        for row in first..last {
            let mut spans = vec![Span::styled(SPINE, self.accent)];
            let mut run = String::new();
            let mut run_pill = false;
            for cell in cells.iter().filter(|cell| cell.row == row) {
                let in_pill = self
                    .pills
                    .iter()
                    .any(|(start, end)| cell.index >= *start && cell.index < *end);
                if in_pill != run_pill && !run.is_empty() {
                    spans.push(styled_run(std::mem::take(&mut run), run_pill, self.accent));
                }
                run_pill = in_pill;
                run.push(cell.ch);
            }
            if !run.is_empty() {
                spans.push(styled_run(run, run_pill, self.accent));
            }
            lines.push(Line::from(spans));
        }
        ComposerRender {
            lines: super::layout::fit_lines(lines, self.width),
            cursor_x: (view::cell_width(SPINE) + cursor_col).min(self.width.saturating_sub(1))
                as u16,
            cursor_y: (cursor_row - first) as u16,
        }
    }
}

fn styled_run(text: String, pill: bool, accent: Style) -> Span<'static> {
    if pill {
        Span::styled(text, accent)
    } else {
        Span::raw(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

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
        assert_eq!(rendered.lines[0].spans[2].content.as_ref(), "[▧ shot.png]");
        assert_eq!(rendered.lines[0].spans[2].style, accent);
    }

    #[test]
    fn long_input_wraps_and_keeps_the_cursor_on_its_row() {
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
        assert_eq!(rendered.lines.len(), 2);
        assert_eq!(text(&rendered.lines[0]), "│ abcdefghi");
        assert_eq!(text(&rendered.lines[1]), "│ jklmno");
        assert_eq!((rendered.cursor_x, rendered.cursor_y), (8, 1));
    }

    #[test]
    fn a_cursor_at_the_end_of_a_full_row_starts_the_next_row() {
        let rendered = Composer::new(
            "abcdefghi",
            9,
            "unused",
            12,
            Style::default(),
            Style::default(),
            &[],
        )
        .render();
        assert_eq!(rendered.lines.len(), 2);
        assert_eq!((rendered.cursor_x, rendered.cursor_y), (2, 1));
    }

    #[test]
    fn very_long_input_scrolls_rows_to_the_cursor() {
        let long: String = (0..90).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        // width 12 → 9 chars per row → 10 rows; only six show.
        let tail = Composer::new(
            &long,
            90,
            "unused",
            12,
            Style::default(),
            Style::default(),
            &[],
        )
        .render();
        assert_eq!(tail.lines.len(), COMPOSER_MAX_ROWS);
        assert_eq!(tail.cursor_y, (COMPOSER_MAX_ROWS - 1) as u16);
        let head = Composer::new(
            &long,
            0,
            "unused",
            12,
            Style::default(),
            Style::default(),
            &[],
        )
        .render();
        assert_eq!(text(&head.lines[0]), "│ abcdefghi");
        assert_eq!(head.cursor_y, 0);
    }

    #[test]
    fn wide_glyphs_wrap_by_cells() {
        let rendered = Composer::new(
            "日本語日本語",
            6,
            "unused",
            11,
            Style::default(),
            Style::default(),
            &[],
        )
        .render();
        assert_eq!(rendered.lines.len(), 2, "8 cells per row, 12 cells of text");
        assert!(rendered.lines.iter().all(|line| line.width() <= 11));
    }
}
