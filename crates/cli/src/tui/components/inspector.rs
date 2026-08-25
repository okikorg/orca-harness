//! Atomic Inspector sections and source previews.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::view;
use crate::view::theme;

pub const INSPECTOR_BODY_INDENT: &str = "  ";

pub struct InspectorSection {
    label: String,
    style: Style,
    rows: Vec<Line<'static>>,
}

impl InspectorSection {
    pub fn new(label: impl Into<String>, style: Style) -> Self {
        Self {
            label: label.into(),
            style,
            rows: Vec::new(),
        }
    }

    pub fn push(&mut self, row: Line<'static>) {
        self.rows.push(row);
    }

    pub fn extend<I>(&mut self, rows: I)
    where
        I: IntoIterator<Item = Line<'static>>,
    {
        self.rows.extend(rows);
    }

    pub fn append_to(self, target: &mut Vec<Line<'static>>) {
        if !target.is_empty() {
            target.push(Line::from(""));
        }
        target.push(Line::from(Span::styled(
            format!("  {}", self.label),
            self.style,
        )));
        target.extend(self.rows);
    }
}

/// Wrapped prose with the same straight spine used by code previews,
/// clarification inputs, and transcript detail rows.
pub fn inspector_text(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    let text = view::sanitize_cells(text);
    let prefix = format!("{INSPECTOR_BODY_INDENT}│");
    let text_prefix = format!("{prefix} ");
    let body_width = width.saturating_sub(text_prefix.chars().count()).max(8);
    let mut lines = Vec::new();
    for source in text.lines() {
        let wrapped = textwrap::wrap(source, body_width);
        if wrapped.is_empty() {
            lines.push(Line::from(Span::styled(prefix.clone(), theme().dim)));
        } else {
            for part in wrapped {
                lines.push(Line::from(vec![
                    Span::styled(text_prefix.clone(), theme().dim),
                    Span::styled(part.into_owned(), style),
                ]));
            }
        }
    }
    lines
}

/// Compact aligned metadata. Values remain plain text so themes and mono mode
/// carry the same information without adding panel chrome.
pub fn inspector_fields<'a, I>(rows: I, width: usize) -> Vec<Line<'static>>
where
    I: IntoIterator<Item = (&'a str, String)>,
{
    let rows: Vec<_> = rows.into_iter().collect();
    let label_width = rows
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(0)
        .min(14);
    rows.into_iter()
        .map(|(label, value)| {
            let prefix = format!("{INSPECTOR_BODY_INDENT}{label:<label_width$}  ");
            Line::from(vec![
                Span::styled(prefix.clone(), theme().dim),
                Span::raw(view::truncate_line(
                    &value,
                    width.saturating_sub(prefix.chars().count()),
                )),
            ])
        })
        .collect()
}

pub struct CodePreview<'a> {
    pub text: &'a str,
    pub language: &'a str,
    pub width: usize,
    pub indent: &'a str,
    pub plain_style: Style,
}

impl CodePreview<'_> {
    pub fn lines(&self) -> Vec<Line<'static>> {
        if self.language == "text" {
            self.text
                .lines()
                .map(|line| {
                    Line::from(Span::styled(
                        format!(
                            "{}{}",
                            self.indent,
                            view::truncate_line(line, self.width.saturating_sub(self.indent.len()))
                        ),
                        self.plain_style,
                    ))
                })
                .collect()
        } else {
            view::highlighted_code_lines(self.text, self.language, self.width, self.indent)
        }
    }
}
