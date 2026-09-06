//! The approval prompt: what a tool wants to do and the keys that decide.

use ratatui::text::{Line, Span};

use super::inspector::spine_text;
use crate::view::glyphs::glyphs;
use crate::view::{self, theme};

pub struct ApprovalPrompt<'a> {
    pub tool_name: &'a str,
    /// Pre-rendered call line, e.g. `shell $ cargo test`.
    pub detail: &'a str,
    /// Plain yes/no: the "always" answers are not offered.
    pub yes_no: bool,
}

const INDENT: &str = "    ";
/// Cells between two key hints on one row.
const HINT_GAP: usize = 3;

impl ApprovalPrompt<'_> {
    pub fn choices(&self) -> &'static [(&'static str, &'static str)] {
        if self.yes_no {
            &[("y", "yes"), ("n", "no")]
        } else {
            &[
                ("y", "allow once"),
                ("a", "always this session"),
                ("A", "always for workspace"),
                ("n", "deny"),
            ]
        }
    }

    /// The status hint: the two ends of the legend, short enough to
    /// survive a narrow bar. The prompt above carries every choice.
    pub fn hint(&self) -> String {
        let choices = self.choices();
        let (first, last) = (choices[0], choices[choices.len() - 1]);
        format!("{} {} · {} {}", first.0, first.1, last.0, last.1)
    }

    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let t = theme();
        let mut lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(format!("  {} ", glyphs().attention), t.warn),
                Span::styled("approval required", t.warn),
                Span::styled(format!(" · {}", self.tool_name), t.dim),
            ]),
        ];
        lines.extend(spine_text(
            self.detail,
            width,
            INDENT,
            ratatui::style::Style::default(),
        ));
        // Key hints flow onto as many rows as the width needs, so a narrow
        // terminal never wraps them mid-word inside the Paragraph.
        let mut row: Vec<Span<'static>> = Vec::new();
        let mut used = 0;
        for (key, label) in self.choices() {
            let hint_width = view::cell_width(key) + 1 + view::cell_width(label);
            let gap = if row.is_empty() { 0 } else { HINT_GAP };
            if !row.is_empty() && INDENT.len() + used + gap + hint_width > width {
                lines.push(Line::from(std::mem::take(&mut row)));
                used = 0;
            }
            if row.is_empty() {
                row.push(Span::raw(INDENT));
            } else {
                row.push(Span::raw(" ".repeat(HINT_GAP)));
                used += HINT_GAP;
            }
            if INDENT.len() + hint_width > width {
                row.clear();
                lines.extend(super::layout::wrapped(
                    label,
                    &format!("{INDENT}{key} "),
                    width,
                    t.strong,
                ));
                used = 0;
                continue;
            }
            row.push(Span::styled((*key).to_string(), t.strong));
            row.push(Span::styled(format!(" {label}"), t.dim));
            used += hint_width;
        }
        if !row.is_empty() {
            lines.push(Line::from(row));
        }
        super::layout::fit_lines(lines, width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt() -> ApprovalPrompt<'static> {
        ApprovalPrompt {
            tool_name: "shell",
            detail: "shell $ cargo test -p orcacode tui::components -- --nocapture",
            yes_no: false,
        }
    }

    #[test]
    fn wide_terminals_keep_every_choice_on_one_row() {
        let lines = prompt().lines(120);
        let last = lines.last().unwrap();
        assert!(last.width() <= 120);
        assert!(lines.iter().all(|line| line.width() <= 120));
        assert_eq!(lines.len(), 4, "blank, header, detail, keys");
    }

    #[test]
    fn narrow_terminals_reflow_the_choices_instead_of_wrapping_them() {
        let lines = prompt().lines(60);
        assert!(lines.iter().all(|line| line.width() <= 60), "{lines:?}");
        let key_rows = lines
            .iter()
            .filter(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content == "y" || span.content == "n")
            })
            .count();
        assert_eq!(key_rows, 2, "the four choices split across two rows");
    }

    #[test]
    fn yes_no_prompts_offer_two_keys() {
        let mut prompt = prompt();
        prompt.yes_no = true;
        assert_eq!(prompt.hint(), "y yes · n no");
        prompt.yes_no = false;
        assert_eq!(prompt.hint(), "y allow once · n deny");
    }
}
