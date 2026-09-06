//! The standard list-picker: the one interface behind /provider,
//! /theme, /view, /extensions, /sessions, the settings menu, and the
//! approvals list. A cursor over a fixed row count with ↑↓/enter
//! navigation, and the shared tray rendering: dim header with key
//! hints, marker column, emphasized selection, width truncation.
//!
//! Overlays may also declare row actions (delete, rename, …): space
//! arms the action strip for the selected row, the action's key fires
//! [`PickerEvent::Action`], and any other key disarms.
//!
//! Rows are cells: a table's first column reads in the normal text
//! colour and the columns after it are dim, so a name stands out from its
//! description; the selected primary cell is bold in `select`. A column
//! with no cap flexes: when the natural widths overflow the row, it gives
//! up cells so the columns after it stay on screen.

use std::cell::Cell;

use crossterm::event::KeyCode;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::view::glyphs::glyphs;
use crate::view::{self, theme};

/// One row action an overlay offers beyond enter-to-select.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickerAction {
    /// The key that fires it while the strip is armed.
    pub key: char,
    pub label: &'static str,
}

/// Cursor state for a fixed-length list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPicker {
    index: usize,
    len: usize,
    actions: &'static [PickerAction],
    /// Space was pressed: the action strip is showing for the selected
    /// row and action keys are live.
    armed: bool,
    /// First row of the window a bounded catalog last showed. The window
    /// moves only when the cursor leaves it, so ↑ does not jump a page.
    /// Set during rendering, which only has `&self`.
    offset: Cell<usize>,
}

/// What a key did to the picker.
pub enum PickerEvent {
    /// Consumed by navigation; the overlay stays open.
    Moved,
    /// Enter or right-arrow activated this row (always in bounds).
    Activated(usize),
    /// An armed action fired on this row (always in bounds).
    Action { key: char, row: usize },
    /// Not a picker key; the caller decides.
    Ignored,
}

impl ListPicker {
    /// The plain dim header every `&str` entry point renders.
    fn plain_header(header: &str) -> Line<'static> {
        Line::from(Span::styled(header.to_string(), theme().dim))
    }

    /// The header as shown: the action strip while armed, the space hint
    /// when actions exist, otherwise the caller's line untouched.
    fn header(&self, header: Line<'static>) -> Line<'static> {
        let t = theme();
        if self.armed {
            let strip: Vec<String> = self
                .actions
                .iter()
                .map(|a| format!("[{}] {}", a.key, a.label))
                .collect();
            return Line::from(Span::styled(
                format!("actions: {} · any other key cancels", strip.join(" · ")),
                t.warn,
            ));
        }
        if self.actions.is_empty() {
            return header;
        }
        let mut header = header;
        header.spans.push(Span::styled(" · space actions", t.dim));
        header
    }

    pub fn new(len: usize) -> Self {
        Self {
            index: 0,
            len,
            actions: &[],
            armed: false,
            offset: Cell::new(0),
        }
    }

    /// A picker preselected on `index` (clamped to the list).
    pub fn with_selected(len: usize, index: usize) -> Self {
        Self {
            index: index.min(len.saturating_sub(1)),
            ..Self::new(len)
        }
    }

    /// Declare the row actions this picker offers; space arms them.
    pub fn actions(mut self, actions: &'static [PickerAction]) -> Self {
        self.actions = actions;
        self
    }

    pub fn index(&self) -> usize {
        self.index.min(self.len.saturating_sub(1))
    }

    /// Move the cursor by a signed offset (page up/down); clamps to the
    /// list the same way ↑↓ navigation does.
    pub fn move_by(&mut self, delta: isize) {
        let index = self.index() as isize + delta;
        self.index = index.max(0).min(self.len.saturating_sub(1) as isize) as usize;
    }

    /// Shrink after the caller removed rows (an approvals revoke, a
    /// session delete); the cursor stays in bounds.
    pub fn set_len(&mut self, len: usize) {
        self.len = len;
        self.index = self.index();
    }

    pub fn on_key(&mut self, code: KeyCode) -> PickerEvent {
        if self.armed {
            self.armed = false;
            if let KeyCode::Char(c) = code {
                if self.len > 0 && self.actions.iter().any(|a| a.key == c) {
                    return PickerEvent::Action {
                        key: c,
                        row: self.index(),
                    };
                }
            }
            // Any non-action key just disarms; navigation resumes on the
            // next press so a stray key never falls through to select.
            return PickerEvent::Moved;
        }
        match code {
            KeyCode::Up => {
                self.index = self.index().saturating_sub(1);
                PickerEvent::Moved
            }
            KeyCode::Down => {
                self.index = (self.index() + 1).min(self.len.saturating_sub(1));
                PickerEvent::Moved
            }
            KeyCode::Enter | KeyCode::Right if self.len > 0 => PickerEvent::Activated(self.index()),
            KeyCode::Enter | KeyCode::Right => PickerEvent::Moved,
            KeyCode::Char(' ') if !self.actions.is_empty() && self.len > 0 => {
                self.armed = true;
                PickerEvent::Moved
            }
            _ => PickerEvent::Ignored,
        }
    }

    /// The shared tray: dim header (key hints), spacer, one row per
    /// item with the selection marker; the selected row is emphasized.
    /// Row text is composed by the caller; marker, styling, and width
    /// truncation live here. With actions declared, the header hints
    /// space; while armed, it becomes the action strip.
    pub fn lines<I>(&self, header: &str, rows: I, width: usize) -> Vec<Line<'static>>
    where
        I: IntoIterator<Item = String>,
    {
        let dim = theme().dim;
        let rows: Vec<Vec<Span<'static>>> = rows
            .into_iter()
            .map(|row| vec![Span::styled(row, dim)])
            .collect();
        let count = rows.len();
        self.render(Self::plain_header(header), None, &rows, width, 0..count)
    }

    /// Render structured rows as aligned columns through the standard
    /// picker tray. Each `(min, max)` pair controls one column; widths
    /// otherwise follow the widest cell. The assembled row still obeys
    /// `width`.
    pub fn table_lines<const N: usize, I>(
        &self,
        header: &str,
        rows: I,
        column_widths: [(usize, usize); N],
        width: usize,
    ) -> Vec<Line<'static>>
    where
        I: IntoIterator<Item = [String; N]>,
    {
        let rows = table_rows(rows, column_widths, width);
        let count = rows.len();
        self.render(Self::plain_header(header), None, &rows, width, 0..count)
    }

    /// A bounded version of [`Self::lines`] for catalogs larger than the
    /// live region. The window follows the cursor and the header exposes
    /// both the selected position and total row count.
    pub fn windowed_lines<I>(
        &self,
        header: &str,
        rows: I,
        width: usize,
        visible_rows: usize,
    ) -> Vec<Line<'static>>
    where
        I: IntoIterator<Item = String>,
    {
        let dim = theme().dim;
        let rows: Vec<Vec<Span<'static>>> = rows
            .into_iter()
            .map(|row| vec![Span::styled(row, dim)])
            .collect();
        let window = self.window(rows.len(), visible_rows);
        self.render(
            Self::plain_header(header),
            Some(rows.len()),
            &rows,
            width,
            window,
        )
    }

    /// The bounded catalog variant of [`Self::table_lines`]. Column
    /// widths are calculated across the full filtered result so they do
    /// not jump while the cursor pages through the window.
    pub fn windowed_table_lines<const N: usize, I>(
        &self,
        header: &str,
        rows: I,
        column_widths: [(usize, usize); N],
        width: usize,
        visible_rows: usize,
    ) -> Vec<Line<'static>>
    where
        I: IntoIterator<Item = [String; N]>,
    {
        self.windowed_table_lines_styled(
            Self::plain_header(header),
            rows,
            column_widths,
            width,
            visible_rows,
        )
    }

    /// [`Self::windowed_table_lines`] under a header the caller styled
    /// span by span (a tab strip, an emphasized title); the tray still
    /// indents it and anchors the position on the right.
    pub fn windowed_table_lines_styled<const N: usize, I>(
        &self,
        header: Line<'static>,
        rows: I,
        column_widths: [(usize, usize); N],
        width: usize,
        visible_rows: usize,
    ) -> Vec<Line<'static>>
    where
        I: IntoIterator<Item = [String; N]>,
    {
        let rows = table_rows(rows, column_widths, width);
        let window = self.window(rows.len(), visible_rows);
        self.render(header, Some(rows.len()), &rows, width, window)
    }

    /// Two visual lines per selectable entry, without changing picker indices.
    /// Each cached row contains a title span followed by its metadata span.
    pub(crate) fn cached_entry_lines(
        &self,
        header: Line<'static>,
        rows: &[Vec<Span<'static>>],
        width: usize,
        height: usize,
    ) -> Vec<Line<'static>> {
        let room = height.saturating_sub(2);
        let entry_height = if room >= 2 { 2 } else { 1 };
        let window = self.window(rows.len(), room / entry_height);
        let mut lines = self.render(header, None, rows, width, 0..0);
        let t = theme();
        for index in window {
            let Some(title) = rows[index].first() else {
                continue;
            };
            let selected = index == self.index();
            lines.push(super::layout::fit(
                Line::from(vec![
                    Span::styled(marker(selected), if selected { t.select } else { t.dim }),
                    Span::styled(
                        title.content.clone(),
                        if selected {
                            t.select.add_modifier(ratatui::style::Modifier::BOLD)
                        } else {
                            title.style
                        },
                    ),
                ]),
                width,
            ));
            if entry_height == 2 {
                let mut spans = vec![Span::raw("    ")];
                spans.extend(rows[index].iter().skip(1).cloned());
                lines.push(super::layout::fit(Line::from(spans), width));
            }
        }
        lines.truncate(height);
        lines
    }

    /// The rows a bounded catalog shows: the last window, moved just far
    /// enough to keep the cursor inside it.
    fn window(&self, len: usize, visible_rows: usize) -> std::ops::Range<usize> {
        let visible_rows = visible_rows.max(1);
        let selected = self.index().min(len.saturating_sub(1));
        let mut first = self.offset.get().min(len.saturating_sub(visible_rows));
        if selected < first {
            first = selected;
        } else if selected >= first + visible_rows {
            first = selected + 1 - visible_rows;
        }
        self.offset.set(first);
        first..(first + visible_rows).min(len)
    }

    /// Header, spacer, and the rows in `window`, each behind its marker.
    /// `total` adds the `selected/total` position to a bounded header.
    fn render(
        &self,
        header: Line<'static>,
        total: Option<usize>,
        rows: &[Vec<Span<'static>>],
        width: usize,
        window: std::ops::Range<usize>,
    ) -> Vec<Line<'static>> {
        let t = theme();
        let selected = self.index();
        let mut header = self.header(header);
        // The header keeps one style throughout, so the indent and the
        // position take the style of its first span.
        let header_style = header.spans.first().map(|span| span.style).unwrap_or(t.dim);
        header.spans.insert(0, Span::styled("  ", header_style));
        if let Some(total) = total {
            let position = if total == 0 {
                "0/0".to_string()
            } else {
                format!("{}/{total}", selected.min(total - 1) + 1)
            };
            let position = view::truncate_line(&position, width.saturating_sub(2).max(1));
            let budget = width.saturating_sub(view::cell_width(&position) + 3);
            header = super::layout::fit(header, budget);
            let pad = width
                .saturating_sub(view::spans_width(&header.spans) + view::cell_width(&position) + 2)
                .max(1);
            header.spans.push(Span::styled(
                format!("{}{position}", " ".repeat(pad)),
                header_style,
            ));
        }
        let mut lines = vec![super::layout::fit(header, width), Line::from("")];
        for (index, row) in rows.iter().enumerate().take(window.end).skip(window.start) {
            let is_selected = index == selected;
            let mut spans = vec![Span::styled(
                marker(is_selected),
                if is_selected { t.select } else { t.dim },
            )];
            spans.extend(row.iter().cloned().enumerate().map(|(column, span)| {
                if is_selected && column == 0 {
                    Span::styled(
                        span.content,
                        t.select.add_modifier(ratatui::style::Modifier::BOLD),
                    )
                } else {
                    span
                }
            }));
            lines.push(super::layout::fit(Line::from(spans), width));
        }
        lines
    }
}

/// Cells between table columns.
const COLUMN_GAP: usize = 2;

/// The cursor column every row starts with: the mark on the cursor row,
/// its space on the others, so bodies line up.
fn marker(selected: bool) -> String {
    format!("  {} ", if selected { glyphs().cursor } else { " " })
}

/// Aligned cells: the first column in the normal text colour, the rest
/// dim, with two cells between columns. Columns take their widest cell
/// within `(min, max)`; when that overflows `width`, the uncapped columns
/// shrink in order (never below their minimum) so a long first cell cannot
/// push the columns after it off the row.
pub(crate) fn table_rows<const N: usize, I>(
    rows: I,
    column_widths: [(usize, usize); N],
    width: usize,
) -> Vec<Vec<Span<'static>>>
where
    I: IntoIterator<Item = [String; N]>,
{
    let dim = theme().dim;
    let rows: Vec<[String; N]> = rows.into_iter().collect();
    let mut widths: [usize; N] = std::array::from_fn(|column| {
        let widest = rows
            .iter()
            .map(|row| view::cell_width(&row[column]))
            .max()
            .unwrap_or(0);
        let (minimum, maximum) = column_widths[column];
        widest.max(minimum).min(maximum.max(minimum))
    });
    let available =
        width.saturating_sub(view::cell_width(&marker(false)) + COLUMN_GAP * N.saturating_sub(1));
    for (column, (minimum, maximum)) in column_widths.into_iter().enumerate() {
        let excess = widths.iter().sum::<usize>().saturating_sub(available);
        if excess == 0 {
            break;
        }
        if maximum == usize::MAX {
            widths[column] = widths[column].saturating_sub(excess).max(minimum);
        }
    }
    rows.into_iter()
        .map(|row| {
            let mut spans = Vec::with_capacity(N);
            for (column, cell) in row.into_iter().enumerate() {
                let cell = view::truncate_line(&cell, widths[column]);
                let pad = if column + 1 < N {
                    widths[column].saturating_sub(view::cell_width(&cell)) + COLUMN_GAP
                } else {
                    0
                };
                let style = if column == 0 { Style::default() } else { dim };
                spans.push(Span::styled(format!("{cell}{}", " ".repeat(pad)), style));
            }
            spans
        })
        .collect()
}

#[cfg(test)]
mod tests;
