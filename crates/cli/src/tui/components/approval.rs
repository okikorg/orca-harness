//! The approval prompt: what a tool wants to do and the keys that decide.
//!
//! The one framed surface in the product, drawn in the warning colour: it
//! is the moment the run is blocked on the person. Inside, the call is
//! wrapped rather than cut, and the answers form a list the arrows move
//! through, the cursor and weight marking the choice with no fill; each
//! answer's letter still fires it directly.
//! The frame is box-drawing only, so mono renders it too.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::view::glyphs::glyphs;
use crate::view::{self, theme};

pub struct ApprovalPrompt<'a> {
    pub tool_name: &'a str,
    /// Pre-rendered call line, e.g. `shell $ cargo test`.
    pub detail: &'a str,
    /// Plain yes/no: the "always" answers are not offered.
    pub yes_no: bool,
    /// The choice the arrows have moved to, if any.
    pub selected: Option<usize>,
}

/// The answers a prompt offers, as (key, label).
pub fn choices(yes_no: bool) -> &'static [(&'static str, &'static str)] {
    if yes_no {
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

/// Cells left of the frame.
const MARGIN: &str = "  ";

impl ApprovalPrompt<'_> {
    pub fn choices(&self) -> &'static [(&'static str, &'static str)] {
        choices(self.yes_no)
    }

    /// The status hint: the two ends of the legend, short enough to
    /// survive a narrow bar. The prompt above carries every choice.
    pub fn hint_pairs(&self) -> Vec<(&'static str, &'static str)> {
        let choices = self.choices();
        vec![choices[0], choices[choices.len() - 1]]
    }

    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let t = theme();
        let frame = t.warn;
        // Frame: margin, `│ `, body, ` │`.
        let outer = width.saturating_sub(view::cell_width(MARGIN)).max(6);
        let inner = outer.saturating_sub(4).max(1);
        let row = |body: Vec<Span<'static>>| -> Line<'static> {
            let mut body = super::layout::fit(Line::from(body), inner);
            let pad = inner.saturating_sub(body.width());
            body.spans.push(Span::raw(" ".repeat(pad)));
            let mut spans = vec![Span::raw(MARGIN), Span::styled("│ ", frame)];
            spans.extend(body.spans);
            spans.push(Span::styled(" │", frame));
            Line::from(spans)
        };

        let title = format!(
            "─ {} Approval required · {}",
            glyphs().attention,
            view::sanitize_cells(self.tool_name)
        );
        // The truncation trims trailing blanks, so the space before the
        // rule is added after it.
        let title = format!("{} ", view::truncate_line(&title, outer.saturating_sub(3)));
        let mut lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::raw(MARGIN),
                Span::styled(
                    format!(
                        "╭{title}{}╮",
                        "─".repeat(outer.saturating_sub(2 + view::cell_width(&title)))
                    ),
                    frame,
                ),
            ]),
        ];
        // The call, wrapped so its end is never lost.
        let detail = view::sanitize_cells(self.detail);
        for source in detail.lines() {
            for part in textwrap::wrap(source, inner.max(1)) {
                lines.push(row(vec![Span::raw(part.into_owned())]));
            }
        }
        lines.push(row(Vec::new()));

        for (index, (key, label)) in self.choices().iter().enumerate() {
            let selected = self.selected == Some(index);
            let lead = format!("{} ", if selected { glyphs().cursor } else { " " });
            let label = sentence_case(label);
            let key_width = view::cell_width(key);
            let text_width = inner
                .saturating_sub(view::cell_width(&lead) + key_width + 1)
                .max(1);
            let label_style = if selected { t.strong } else { Style::default() };
            for (part_index, part) in textwrap::wrap(&label, text_width).into_iter().enumerate() {
                let part = part.into_owned();
                let mut body = vec![
                    Span::styled(
                        if part_index == 0 {
                            lead.clone()
                        } else {
                            " ".repeat(view::cell_width(&lead))
                        },
                        if selected { t.accent } else { t.dim },
                    ),
                    Span::styled(part.clone(), label_style),
                ];
                if part_index == 0 {
                    let pad = text_width.saturating_sub(view::cell_width(&part)) + 1;
                    body.push(Span::raw(" ".repeat(pad)));
                    body.push(Span::styled((*key).to_string(), t.key));
                }
                lines.push(row(body));
            }
        }

        // The foot carries the list's own keys inside the rule.
        let legend = [("↑↓", "choose"), ("enter", "confirm"), ("esc", "deny")];
        let mut legend_spans = vec![Span::raw(" ")];
        legend_spans.extend(super::keys::hint_spans(&legend));
        legend_spans.push(Span::raw(" "));
        let legend_width = view::spans_width(&legend_spans);
        let mut foot = vec![Span::raw(MARGIN), Span::styled("╰", frame)];
        if legend_width + 4 <= outer {
            let rule = outer - 2 - legend_width;
            foot.push(Span::styled("─".repeat(rule.saturating_sub(1)), frame));
            foot.extend(legend_spans);
            foot.push(Span::styled("─╯", frame));
        } else {
            foot.push(Span::styled(
                format!("{}╯", "─".repeat(outer.saturating_sub(2))),
                frame,
            ));
        }
        lines.push(Line::from(foot));
        super::layout::fit_lines(lines, width)
    }
}

fn sentence_case(label: &str) -> String {
    let mut chars = label.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
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
            selected: None,
        }
    }

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    #[test]
    fn every_choice_gets_a_row_with_its_key_inside_one_frame() {
        let lines = prompt().lines(80);
        let rows = text(&lines);
        assert!(
            lines[1..].iter().all(|line| line.width() == 80),
            "{rows:#?}"
        );
        assert!(rows[1].trim_start().starts_with('╭'), "{rows:#?}");
        assert!(
            rows.last().unwrap().trim_start().starts_with('╰'),
            "{rows:#?}"
        );
        for (key, label) in prompt().choices() {
            let row = rows
                .iter()
                .find(|row| row.contains(&sentence_case(label)))
                .unwrap_or_else(|| panic!("{label}: {rows:#?}"));
            assert!(row.trim_end().ends_with(&format!("{key} │")), "{row:?}");
        }
        assert!(
            rows.iter().any(|row| row.contains("--nocapture")),
            "call kept whole"
        );
    }

    #[test]
    fn narrow_terminals_wrap_labels_and_the_call_instead_of_cutting_them() {
        let lines = prompt().lines(30);
        let rows = text(&lines);
        assert!(lines.iter().all(|line| line.width() <= 30), "{rows:#?}");
        let joined = rows.join(" ");
        assert!(joined.contains("workspace"), "{rows:#?}");
        assert!(joined.contains("--nocapture"), "{rows:#?}");
    }

    #[test]
    fn the_selected_choice_carries_the_cursor() {
        let mut prompt = prompt();
        prompt.selected = Some(3);
        let rows = text(&prompt.lines(80));
        let deny = rows.iter().find(|row| row.contains("Deny")).unwrap();
        assert!(
            deny.contains(&format!("{} Deny", glyphs().cursor)),
            "{deny:?}"
        );
        let once = rows.iter().find(|row| row.contains("Allow once")).unwrap();
        assert!(!once.contains(glyphs().cursor), "{once:?}");
    }

    #[test]
    fn yes_no_prompts_offer_two_keys() {
        let mut prompt = prompt();
        prompt.yes_no = true;
        assert_eq!(prompt.hint_pairs(), [("y", "yes"), ("n", "no")]);
        prompt.yes_no = false;
        assert_eq!(prompt.hint_pairs(), [("y", "allow once"), ("n", "deny")]);
    }
}
