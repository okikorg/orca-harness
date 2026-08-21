//! The standard list-picker: the one interface behind /provider,
//! /theme, /view, /extensions, /sessions, the settings menu, and the
//! approvals list. A cursor over a fixed row count with ↑↓/enter
//! navigation, and the shared tray rendering: dim header with key
//! hints, marker column, emphasized selection, width truncation.
//!
//! Overlays may also declare row actions (delete, rename, …): space
//! arms the action strip for the selected row, the action's key fires
//! [`PickerEvent::Action`], and any other key disarms.

use crossterm::event::KeyCode;
use ratatui::text::{Line, Span};

use crate::view::{self, theme};

/// One row action an overlay offers beyond enter-to-select.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickerAction {
    /// The key that fires it while the strip is armed.
    pub key: char,
    pub label: &'static str,
}

/// Cursor state for a fixed-length list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListPicker {
    index: usize,
    len: usize,
    actions: &'static [PickerAction],
    /// Space was pressed: the action strip is showing for the selected
    /// row and action keys are live.
    armed: bool,
}

/// What a key did to the picker.
pub enum PickerEvent {
    /// Consumed by navigation; the overlay stays open.
    Moved,
    /// Enter activated this row (always in bounds).
    Activated(usize),
    /// An armed action fired on this row (always in bounds).
    Action { key: char, row: usize },
    /// Not a picker key; the caller decides.
    Ignored,
}

impl ListPicker {
    pub fn new(len: usize) -> Self {
        Self {
            index: 0,
            len,
            actions: &[],
            armed: false,
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
            KeyCode::Enter if self.len > 0 => PickerEvent::Activated(self.index()),
            KeyCode::Enter => PickerEvent::Moved,
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
        let header_style = if self.armed { t.warn } else { t.dim };
        let mut lines = vec![
            Line::from(Span::styled(format!("  {header}"), header_style)),
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
}
