//! The standard list-picker: the one interface behind /provider,
//! /theme, /view, /extensions, /sessions, the settings menu, and the
//! approvals list. A cursor over a fixed row count with ↑↓/enter
//! navigation, and the shared tray rendering: dim header with key
//! hints, marker column, emphasized selection, width truncation.

use crossterm::event::KeyCode;
use ratatui::text::{Line, Span};

use crate::view::{self, theme};

/// Cursor state for a fixed-length list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListPicker {
    index: usize,
    len: usize,
}

/// What a key did to the picker.
pub enum PickerEvent {
    /// Consumed by navigation; the overlay stays open.
    Moved,
    /// Enter activated this row (always in bounds).
    Activated(usize),
    /// Not a picker key; the caller decides.
    Ignored,
}

impl ListPicker {
    pub fn new(len: usize) -> Self {
        Self { index: 0, len }
    }

    /// A picker preselected on `index` (clamped to the list).
    pub fn with_selected(len: usize, index: usize) -> Self {
        Self {
            index: index.min(len.saturating_sub(1)),
            len,
        }
    }

    pub fn index(&self) -> usize {
        self.index.min(self.len.saturating_sub(1))
    }

    /// Shrink after the caller removed rows (an approvals revoke); the
    /// cursor stays in bounds.
    pub fn set_len(&mut self, len: usize) {
        self.len = len;
        self.index = self.index();
    }

    pub fn on_key(&mut self, code: KeyCode) -> PickerEvent {
        match code {
            KeyCode::Up => {
                self.index = self.index().saturating_sub(1);
                PickerEvent::Moved
            }
            KeyCode::Down => {
                self.index = (self.index() + 1).min(self.len.saturating_sub(1));
                PickerEvent::Moved
            }
            KeyCode::Enter if self.len > 0 => PickerEvent::Activated(self.index()),
            KeyCode::Enter => PickerEvent::Moved,
            _ => PickerEvent::Ignored,
        }
    }

    /// The shared tray: dim header (key hints), spacer, one row per
    /// item with the selection marker; the selected row is emphasized.
    /// Row text is composed by the caller; marker, styling, and width
    /// truncation live here.
    pub fn lines<I>(&self, header: &str, rows: I, width: usize) -> Vec<Line<'static>>
    where
        I: IntoIterator<Item = String>,
    {
        let t = theme();
        let mut lines = vec![
            Line::from(Span::styled(format!("  {header}"), t.dim)),
            Line::from(""),
        ];
        let selected = self.index();
        for (index, row) in rows.into_iter().enumerate() {
            let marker = if index == selected { "▸ " } else { "  " };
            let style = if index == selected { t.strong } else { t.dim };
            lines.push(Line::from(Span::styled(
                view::truncate_line(&format!("  {marker}{row}"), width),
                style,
            )));
        }
        lines
    }
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
    }

    #[test]
    fn enter_activates_the_selected_row() {
        let mut picker = ListPicker::with_selected(3, 1);
        assert!(matches!(
            picker.on_key(KeyCode::Enter),
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
    fn other_keys_are_ignored() {
        let mut picker = ListPicker::new(2);
        assert!(matches!(
            picker.on_key(KeyCode::Char('x')),
            PickerEvent::Ignored
        ));
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
}
