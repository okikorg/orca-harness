//! Key hints in one voice: the key in the theme's key style, its action
//! dim, two cells between pairs. Every legend (approval, ask, pickers,
//! the composer) goes through here so none of them drift.

use ratatui::text::{Line, Span};

use crate::view::{self, theme};

/// Cells between two hints.
const GAP: &str = "  ";

/// The words that open a key hint in a `·`-joined header. Only these are
/// set as keys, so a header's notes (`type to filter`, `saved/live`) stay
/// plain text.
const KEY_WORDS: &[&str] = &[
    "↑↓",
    "←→",
    "←",
    "→",
    "→/enter",
    "enter",
    "enter/space",
    "esc",
    "space",
    "tab",
    "PgUp/PgDn",
    "pgdn",
    "ctrl+y",
    "ctrl+o",
];

/// A tray or picker header written as `Title · note · key action · …`,
/// as spans: the title and notes in `base`, each hint's key in the key
/// style, and consecutive hints joined by two cells like every other
/// legend. Order is kept.
pub fn header_spans(text: &str, base: ratatui::style::Style) -> Vec<Span<'static>> {
    let t = theme();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut last_was_hint = false;
    for (index, piece) in text.split(" · ").enumerate() {
        let hint = (index > 0)
            .then(|| piece.split_once(' '))
            .flatten()
            .filter(|(key, _)| KEY_WORDS.contains(key));
        if index > 0 {
            let gap = if hint.is_some() && last_was_hint {
                GAP
            } else {
                " · "
            };
            spans.push(Span::styled(gap, base));
        }
        match hint {
            Some((key, action)) => {
                spans.push(Span::styled(key.to_string(), t.key));
                spans.push(Span::styled(format!(" {action}"), base));
            }
            None => spans.push(Span::styled(piece.to_string(), base)),
        }
        last_was_hint = hint.is_some();
    }
    spans
}

/// `key action  key action`, as spans. An empty key leaves a plain dim
/// note (`scrolled`) in the same row.
pub fn hint_spans(hints: &[(&str, &str)]) -> Vec<Span<'static>> {
    let t = theme();
    let mut spans = Vec::with_capacity(hints.len() * 3);
    for (index, (key, action)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(GAP));
        }
        if key.is_empty() {
            spans.push(Span::styled((*action).to_string(), t.dim));
            continue;
        }
        spans.push(Span::styled((*key).to_string(), t.key));
        spans.push(Span::styled(format!(" {action}"), t.dim));
    }
    spans
}

/// The hints behind `indent`, flowed onto as many rows as `width` needs.
/// A pair never splits across rows.
pub fn hint_rows(hints: &[(&str, &str)], indent: &str, width: usize) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut row: Vec<(&str, &str)> = Vec::new();
    let pair_width = |(key, action): &(&str, &str)| {
        view::cell_width(key) + usize::from(!key.is_empty()) + view::cell_width(action)
    };
    let mut used = view::cell_width(indent);
    for hint in hints {
        let gap = if row.is_empty() { 0 } else { GAP.len() };
        if !row.is_empty() && used + gap + pair_width(hint) > width {
            rows.push(indented(indent, &row, width));
            row.clear();
            used = view::cell_width(indent);
        }
        used += if row.is_empty() { 0 } else { GAP.len() } + pair_width(hint);
        row.push(*hint);
    }
    if !row.is_empty() {
        rows.push(indented(indent, &row, width));
    }
    rows
}

fn indented(indent: &str, hints: &[(&str, &str)], width: usize) -> Line<'static> {
    let mut spans = vec![Span::raw(indent.to_string())];
    spans.extend(hint_spans(hints));
    super::layout::fit(Line::from(spans), width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_take_the_key_style_and_actions_stay_dim() {
        let t = theme();
        let spans = hint_spans(&[("enter", "send"), ("esc", "cancel")]);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "enter send  esc cancel");
        assert_eq!(spans[0].style, t.key);
        assert_eq!(spans[1].style, t.dim);
    }

    #[test]
    fn a_header_sets_known_keys_and_leaves_notes_plain() {
        let t = theme();
        let spans = header_spans(
            "Skills · type to filter · →/enter invoke · esc close",
            t.dim,
        );
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Skills · type to filter · →/enter invoke  esc close");
        let keys: Vec<&str> = spans
            .iter()
            .filter(|s| s.style == t.key)
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(keys, ["→/enter", "esc"]);
    }

    #[test]
    fn rows_reflow_without_splitting_a_pair() {
        let rows = hint_rows(
            &[
                ("↑↓", "move"),
                ("space", "select"),
                ("tab", "next topic"),
                ("esc", "cancel"),
            ],
            "  ",
            24,
        );
        assert!(rows.len() > 1);
        assert!(rows.iter().all(|row| row.width() <= 24));
        let text: Vec<String> = rows.iter().map(|row| row.to_string()).collect();
        assert!(
            text.iter().any(|row| row.contains("tab next topic")),
            "{text:?}"
        );
    }
}
