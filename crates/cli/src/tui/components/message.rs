//! User and assistant transcript messages.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::view;

pub fn user_prompt(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    let indent = "┃ ";
    let body_width = width.saturating_sub(indent.len()).max(16);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.trim().is_empty() {
            lines.push(Line::from(""));
            continue;
        }
        for piece in textwrap::wrap(paragraph, body_width) {
            lines.push(Line::from(Span::styled(format!("{indent}{piece}"), style)));
        }
    }
    lines
}

pub fn assistant_message(text: &str, width: usize) -> Vec<Line<'static>> {
    view::markdown_lines(text, width, "  ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_wrapped_user_row_keeps_the_spine() {
        let lines = user_prompt("one two three four five six", 12, Style::default());
        assert!(lines.len() > 1);
        assert!(lines
            .iter()
            .all(|line| line.spans[0].content.starts_with("┃ ")));
    }
}
