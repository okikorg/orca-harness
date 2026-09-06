//! A tab strip header: the tab labels in a row, the current one in the
//! strong style and the rest dim, with the tray's key hint after them.

use ratatui::text::{Line, Span};

use crate::view::theme;

/// `A · B · C · hint`, with `labels[active]` emphasized. An empty hint
/// leaves the strip ending at the last tab.
pub fn tab_strip(labels: &[&str], active: usize, hint: &str) -> Line<'static> {
    let t = theme();
    let mut spans = Vec::with_capacity(labels.len() * 2 + 1);
    for (index, label) in labels.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", t.dim));
        }
        let style = if index == active {
            t.strong.add_modifier(ratatui::style::Modifier::BOLD)
        } else {
            t.dim
        };
        spans.push(Span::styled(label.to_string(), style));
    }
    if !hint.is_empty() {
        spans.push(Span::styled(format!(" · {hint}"), t.dim));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::theme;

    #[test]
    fn strip_emphasizes_the_active_tab_and_dims_the_rest() {
        let t = theme();
        let line = tab_strip(&["Running", "Done", "All"], 1, "tab switch");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Running · Done · All · tab switch");
        let done = line
            .spans
            .iter()
            .find(|span| span.content == "Done")
            .expect("active label is its own span");
        assert_eq!(
            done.style,
            t.strong.add_modifier(ratatui::style::Modifier::BOLD)
        );
        assert!(line
            .spans
            .iter()
            .filter(|span| span.content != "Done")
            .all(|span| span.style == t.dim));
    }

    #[test]
    fn strip_without_a_hint_ends_at_the_last_tab() {
        let line = tab_strip(&["A", "B"], 0, "");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "A · B");
    }
}
