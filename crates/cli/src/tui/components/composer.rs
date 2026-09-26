//! The composer: a spine, the prompt text wrapped to the width, image
//! pills, and the cursor. Long input grows the composer a row at a time
//! up to [`COMPOSER_MAX_ROWS`]; past that the rows scroll to keep the
//! cursor in view, and an arrow in the spine of an edge row says text is
//! out of sight that way.
//!
//! The spine's colour says what enter will do (the caller picks it: the
//! accent to send, dim while a turn runs and input queues, the warning
//! colour in yolo), and a leading slash command the catalog knows is
//! set in the accent, like a pill.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::view;

/// Rows the composer may occupy before it scrolls.
pub const COMPOSER_MAX_ROWS: usize = 6;
const SPINE: &str = "│ ";
/// The spine of the top row when rows are hidden above it.
const SCROLL_UP: &str = "↑ ";
/// The spine of the bottom row when rows are hidden below it.
const SCROLL_DOWN: &str = "↓ ";

pub struct Composer<'a> {
    text: &'a str,
    cursor: usize,
    placeholder: &'a str,
    width: usize,
    accent: Style,
    dim: Style,
    pills: &'a [(usize, usize)],
    /// The spine's style; the accent unless the caller says otherwise.
    spine: Style,
    /// Char range of a recognised leading `/command`.
    command: Option<(usize, usize)>,
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
            spine: accent,
            command: None,
        }
    }

    /// Colour the spine for the composer's state.
    pub fn spine(mut self, style: Style) -> Self {
        self.spine = style;
        self
    }

    /// Mark `[start, end)` (chars) as a recognised slash command.
    pub fn command(mut self, range: Option<(usize, usize)>) -> Self {
        self.command = range;
        self
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
                        Span::styled(SPINE, self.spine),
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
        let command_style = self.accent.add_modifier(ratatui::style::Modifier::BOLD);
        for row in first..last {
            let mut spans = vec![Span::styled(SPINE, self.spine)];
            let mut run = String::new();
            let mut run_kind = RunKind::Plain;
            for cell in cells.iter().filter(|cell| cell.row == row) {
                let kind = if self
                    .pills
                    .iter()
                    .any(|(start, end)| cell.index >= *start && cell.index < *end)
                {
                    RunKind::Pill
                } else if self
                    .command
                    .is_some_and(|(start, end)| cell.index >= start && cell.index < end)
                {
                    RunKind::Command
                } else {
                    RunKind::Plain
                };
                if kind != run_kind && !run.is_empty() {
                    spans.push(run_kind.span(std::mem::take(&mut run), self.accent, command_style));
                }
                run_kind = kind;
                run.push(cell.ch);
            }
            if !run.is_empty() {
                spans.push(run_kind.span(run, self.accent, command_style));
            }
            lines.push(Line::from(spans));
        }
        // Rows out of sight are flagged in the spine of the edge row, so
        // scrolled text is never a surprise. The spine cell is the one
        // place a full row always has room for it.
        if first > 0 {
            lines[0].spans[0] = Span::styled(SCROLL_UP, self.spine);
        }
        if last < rows {
            let index = lines.len() - 1;
            lines[index].spans[0] = Span::styled(SCROLL_DOWN, self.spine);
        }
        ComposerRender {
            lines: super::layout::fit_lines(lines, self.width),
            cursor_x: (view::cell_width(SPINE) + cursor_col).min(self.width.saturating_sub(1))
                as u16,
            cursor_y: (cursor_row - first) as u16,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunKind {
    Plain,
    Pill,
    Command,
}

impl RunKind {
    fn span(self, text: String, accent: Style, command: Style) -> Span<'static> {
        match self {
            Self::Plain => Span::raw(text),
            Self::Pill => Span::styled(text, accent),
            Self::Command => Span::styled(text, command),
        }
    }
}

/// The char range of a leading `/command` when `is_command` knows the
/// word, for [`Composer::command`].
pub fn command_range(text: &str, is_command: impl Fn(&str) -> bool) -> Option<(usize, usize)> {
    let rest = text.strip_prefix('/')?;
    let word: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    (!word.is_empty() && is_command(&word)).then(|| (0, 1 + word.chars().count()))
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
    fn hidden_rows_are_flagged_in_the_spine_of_the_edge_rows() {
        let long: String = (0..90).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        // width 30 → 27 cells a row; 90 chars → 4 rows, all visible.
        let fits =
            Composer::new(&long, 90, "", 30, Style::default(), Style::default(), &[]).render();
        assert!(fits.lines.iter().all(|line| text(line).starts_with(SPINE)));
        let long = long.repeat(3);
        let tail =
            Composer::new(&long, 270, "", 30, Style::default(), Style::default(), &[]).render();
        assert!(text(&tail.lines[0]).starts_with(SCROLL_UP));
        assert!(text(tail.lines.last().unwrap()).starts_with(SPINE));
        let head =
            Composer::new(&long, 0, "", 30, Style::default(), Style::default(), &[]).render();
        assert!(text(&head.lines[0]).starts_with(SPINE));
        assert!(text(head.lines.last().unwrap()).starts_with(SCROLL_DOWN));
        assert!(head.lines.iter().all(|line| line.width() <= 30));
    }

    #[test]
    fn a_known_slash_command_takes_the_accent_and_the_spine_takes_its_state() {
        let accent = Style::default().fg(ratatui::style::Color::Cyan);
        let dim = Style::default().fg(ratatui::style::Color::DarkGray);
        let text = "/models gpt";
        let range = command_range(text, |word| word == "models");
        assert_eq!(range, Some((0, 7)));
        assert_eq!(command_range("/nope x", |word| word == "models"), None);
        assert_eq!(command_range("models", |_| true), None);
        let rendered = Composer::new(text, 11, "", 40, accent, dim, &[])
            .spine(dim)
            .command(range)
            .render();
        let spans = &rendered.lines[0].spans;
        assert_eq!(spans[0].style, dim, "spine follows the state");
        assert_eq!(spans[1].content.as_ref(), "/models");
        assert!(spans[1]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
        assert_eq!(spans[1].style.fg, accent.fg);
        assert_eq!(spans[2].content.as_ref(), " gpt");
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
