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
//! description; the cursor row is painted `select` throughout.

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
    fn header(&self, header: &str) -> (String, Style) {
        let t = theme();
        let header = if self.armed {
            let strip: Vec<String> = self
                .actions
                .iter()
                .map(|a| format!("[{}] {}", a.key, a.label))
                .collect();
            format!("actions: {} · any other key cancels", strip.join(" · "))
        } else if self.actions.is_empty() {
            header.to_string()
        } else {
            format!("{header} · space actions")
        };
        let style = if self.armed { t.warn } else { t.dim };
        (header, style)
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
        self.render(header, None, rows, width, 0..count)
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
        let rows = table_rows(rows, column_widths);
        let count = rows.len();
        self.render(header, None, rows, width, 0..count)
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
        self.render(header, Some(rows.len()), rows, width, window)
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
        let rows = table_rows(rows, column_widths);
        let window = self.window(rows.len(), visible_rows);
        self.render(header, Some(rows.len()), rows, width, window)
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
        header: &str,
        total: Option<usize>,
        rows: Vec<Vec<Span<'static>>>,
        width: usize,
        window: std::ops::Range<usize>,
    ) -> Vec<Line<'static>> {
        let t = theme();
        let selected = self.index();
        let (header, header_style) = self.header(header);
        let header = match total {
            Some(total) => {
                let position = if total == 0 {
                    "0/0".to_string()
                } else {
                    format!("{}/{total}", selected.min(total - 1) + 1)
                };
                let pad = width
                    .saturating_sub(view::cell_width(&header) + view::cell_width(&position) + 4)
                    .max(1);
                format!("  {header}{}{position}", " ".repeat(pad))
            }
            None => format!("  {header}"),
        };
        let mut lines = vec![
            Line::from(Span::styled(header, header_style)),
            Line::from(""),
        ];
        for (index, row) in rows
            .into_iter()
            .enumerate()
            .take(window.end)
            .skip(window.start)
        {
            let is_selected = index == selected;
            let marker = format!("  {} ", if is_selected { glyphs().cursor } else { " " });
            let mut spans = vec![Span::styled(
                marker,
                if is_selected { t.select } else { t.dim },
            )];
            spans.extend(row.into_iter().map(|span| {
                if is_selected {
                    Span::styled(span.content, t.select)
                } else {
                    span
                }
            }));
            lines.push(Line::from(view::truncate_styled_line(spans, width)));
        }
        lines
    }
}

/// Aligned cells: the first column in the normal text colour, the rest
/// dim, with two cells between columns.
fn table_rows<const N: usize, I>(
    rows: I,
    column_widths: [(usize, usize); N],
) -> Vec<Vec<Span<'static>>>
where
    I: IntoIterator<Item = [String; N]>,
{
    let dim = theme().dim;
    let rows: Vec<[String; N]> = rows.into_iter().collect();
    let widths: [usize; N] = std::array::from_fn(|column| {
        let widest = rows
            .iter()
            .map(|row| view::cell_width(&row[column]))
            .max()
            .unwrap_or(0);
        let (minimum, maximum) = column_widths[column];
        widest.max(minimum).min(maximum.max(minimum))
    });
    rows.into_iter()
        .map(|row| {
            let mut spans = Vec::with_capacity(N);
            for (column, cell) in row.into_iter().enumerate() {
                let cell = view::truncate_line(&cell, widths[column]);
                let pad = if column + 1 < N {
                    widths[column].saturating_sub(view::cell_width(&cell)) + 2
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
mod tests {
    use super::*;

    #[test]
    fn navigation_clamps_to_the_list() {
        let mut picker = ListPicker::new(3);
        assert!(matches!(picker.on_key(KeyCode::Up), PickerEvent::Moved));
        assert_eq!(picker.index(), 0);
        picker.on_key(KeyCode::Down);
        picker.on_key(KeyCode::Down);
        picker.on_key(KeyCode::Down);
        assert_eq!(picker.index(), 2);
        assert!(matches!(
            picker.on_key(KeyCode::Right),
            PickerEvent::Activated(2)
        ));
    }

    #[test]
    fn enter_activates_the_selected_row() {
        let mut picker = ListPicker::with_selected(3, 1);
        assert!(matches!(
            picker.on_key(KeyCode::Enter),
            PickerEvent::Activated(1)
        ));
        // Right and enter share activation semantics.
        let mut picker = ListPicker::with_selected(3, 1);
        assert!(matches!(
            picker.on_key(KeyCode::Right),
            PickerEvent::Activated(1)
        ));
    }

    #[test]
    fn enter_on_an_empty_list_never_activates() {
        let mut picker = ListPicker::new(0);
        assert!(matches!(picker.on_key(KeyCode::Enter), PickerEvent::Moved));
    }

    #[test]
    fn preselection_and_shrink_stay_in_bounds() {
        let picker = ListPicker::with_selected(3, 9);
        assert_eq!(picker.index(), 2);
        let mut picker = ListPicker::with_selected(3, 2);
        picker.set_len(1);
        assert_eq!(picker.index(), 0);
    }

    #[test]
    fn move_by_pages_and_clamps_like_arrow_navigation() {
        let mut picker = ListPicker::with_selected(12, 0);
        picker.move_by(5);
        assert_eq!(picker.index(), 5);
        picker.move_by(5);
        assert_eq!(picker.index(), 10);
        // Past the end clamps to the last row; past the top clamps to 0.
        picker.move_by(99);
        assert_eq!(picker.index(), 11);
        picker.move_by(-99);
        assert_eq!(picker.index(), 0);
    }

    #[test]
    fn other_keys_are_ignored() {
        let mut picker = ListPicker::new(2);
        assert!(matches!(
            picker.on_key(KeyCode::Char('x')),
            PickerEvent::Ignored
        ));
    }

    const ACTIONS: &[PickerAction] = &[PickerAction {
        key: 'd',
        label: "delete",
    }];

    #[test]
    fn space_arms_and_the_action_key_fires() {
        let mut picker = ListPicker::with_selected(3, 1).actions(ACTIONS);
        assert!(matches!(
            picker.on_key(KeyCode::Char(' ')),
            PickerEvent::Moved
        ));
        assert!(matches!(
            picker.on_key(KeyCode::Char('d')),
            PickerEvent::Action { key: 'd', row: 1 }
        ));
        // Disarmed again: 'd' is no longer live.
        assert!(matches!(
            picker.on_key(KeyCode::Char('d')),
            PickerEvent::Ignored
        ));
    }

    #[test]
    fn any_other_key_disarms_without_selecting() {
        let mut picker = ListPicker::new(3).actions(ACTIONS);
        picker.on_key(KeyCode::Char(' '));
        assert!(matches!(picker.on_key(KeyCode::Enter), PickerEvent::Moved));
        // The stray enter neither selected nor fired an action; normal
        // navigation resumes.
        assert!(matches!(
            picker.on_key(KeyCode::Enter),
            PickerEvent::Activated(0)
        ));
    }

    #[test]
    fn space_is_ignored_without_actions_or_rows() {
        let mut picker = ListPicker::new(3);
        assert!(matches!(
            picker.on_key(KeyCode::Char(' ')),
            PickerEvent::Ignored
        ));
        let mut picker = ListPicker::new(0).actions(ACTIONS);
        assert!(matches!(
            picker.on_key(KeyCode::Char(' ')),
            PickerEvent::Ignored
        ));
    }

    #[test]
    fn armed_header_shows_the_action_strip() {
        let mut picker = ListPicker::new(2).actions(ACTIONS);
        let text = |p: &ListPicker| -> String {
            p.lines("Header · esc close", ["a".into(), "b".into()], 120)[0]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect()
        };
        assert_eq!(text(&picker), "  Header · esc close · space actions");
        picker.on_key(KeyCode::Char(' '));
        assert_eq!(
            text(&picker),
            "  actions: [d] delete · any other key cancels"
        );
    }

    #[test]
    fn lines_mark_the_selection() {
        let picker = ListPicker::with_selected(2, 1);
        let lines = picker.lines("Header · esc close", ["a".into(), "b".into()], 80);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(text[0], "  Header · esc close");
        assert!(text[2].contains("  a"), "unselected row: {}", text[2]);
        assert!(text[3].contains("▸ b"), "selected row: {}", text[3]);
    }

    #[test]
    fn windowed_lines_move_the_window_only_when_the_cursor_leaves_it() {
        let mut picker = ListPicker::with_selected(20, 0);
        let rows = || (1..=20).map(|n| format!("row {n}"));
        let text = |lines: Vec<Line<'_>>| -> Vec<String> {
            lines
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect()
                })
                .collect()
        };
        // Moving down inside the window leaves it where it is.
        picker.on_key(KeyCode::Down);
        let shown = text(picker.windowed_lines("Catalog", rows(), 80, 5));
        assert!(shown[2].contains("row 1"), "{shown:?}");
        assert!(shown[3].contains("▸ row 2"), "{shown:?}");
        // Leaving the window at the foot scrolls by one row, not a page.
        for _ in 0..4 {
            picker.on_key(KeyCode::Down);
        }
        let shown = text(picker.windowed_lines("Catalog", rows(), 80, 5));
        assert!(shown[2].contains("row 2"), "{shown:?}");
        assert!(shown[6].contains("▸ row 6"), "{shown:?}");
        // Coming back up keeps the same window until the cursor leaves it.
        picker.on_key(KeyCode::Up);
        let shown = text(picker.windowed_lines("Catalog", rows(), 80, 5));
        assert!(shown[2].contains("row 2"), "{shown:?}");
        assert!(shown[5].contains("▸ row 5"), "{shown:?}");
    }

    #[test]
    fn table_columns_after_the_first_are_dim_and_the_cursor_row_is_select() {
        let picker = ListPicker::with_selected(2, 1);
        let rows = [
            ["name".into(), "description".into()],
            ["other".into(), "text".into()],
        ];
        let lines = picker.table_lines("Catalog", rows, [(0, 10), (0, usize::MAX)], 80);
        let t = theme();
        // Unselected: marker dim, name plain, description dim.
        assert_eq!(lines[2].spans[0].style, t.dim);
        assert_eq!(lines[2].spans[1].style, Style::default());
        assert_eq!(lines[2].spans[2].style, t.dim);
        // Selected: everything in the select colour.
        assert!(lines[3].spans.iter().all(|span| span.style == t.select));
    }

    #[test]
    fn windowed_lines_keep_a_large_list_cursor_visible_and_show_position() {
        let picker = ListPicker::with_selected(20, 12);
        let rows = (1..=20).map(|n| format!("row {n}"));
        let lines = picker.windowed_lines("Catalog", rows, 80, 5);
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert!(text[0].contains("13/20"), "{}", text[0]);
        assert_eq!(text.len(), 7);
        assert!(text.iter().any(|line| line.contains("▸ row 13")));
        assert!(!text.iter().any(|line| line.contains("row 8")));
    }

    #[test]
    fn table_lines_align_columns_and_cap_long_cells() {
        let picker = ListPicker::with_selected(2, 1);
        let rows = [
            ["short".into(), "first description".into()],
            ["a-very-long-name".into(), "second description".into()],
        ];
        let lines = picker.table_lines("Catalog", rows, [(0, 10), (0, usize::MAX)], 80);
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        let first_column = text[2][..text[2].find("first description").unwrap()]
            .chars()
            .count();
        let second_column = text[3][..text[3].find("second description").unwrap()]
            .chars()
            .count();
        assert_eq!(first_column, second_column);
        assert!(text[3].contains("a-very-lo…"), "{}", text[3]);
        assert!(text[3].contains("▸ "), "{}", text[3]);
    }

    #[test]
    fn windowed_table_lines_keep_columns_stable_across_pages() {
        let picker = ListPicker::with_selected(12, 10);
        let rows = (0..12).map(|index| {
            [
                if index == 0 {
                    "widest-name".to_string()
                } else {
                    format!("s{index}")
                },
                format!("description {index}"),
            ]
        });
        let lines = picker.windowed_table_lines("Catalog", rows, [(0, 20), (0, usize::MAX)], 80, 5);
        let selected: String = lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains("description 10"))
            })
            .unwrap()
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();

        let detail_column = selected[..selected.find("description 10").unwrap()]
            .chars()
            .count();
        assert_eq!(detail_column, 17, "{selected}");
        assert!(selected.contains("▸ s10"), "{selected}");
        assert!(lines[0]
            .spans
            .iter()
            .any(|span| span.content.contains("11/12")));
    }
}
