//! Inspector prose, field and source-preview helpers. Blocks themselves
//! are [`super::section::Section::inspector`].

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::view;
use crate::view::theme;

pub const INSPECTOR_BODY_INDENT: &str = "  ";
/// Widest metadata label before it is truncated to keep values aligned.
const INSPECTOR_LABEL_CAP: usize = 14;

/// Wrapped prose with the same straight spine used by code previews,
/// clarification inputs, and transcript detail rows.
pub fn inspector_text(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    spine_text(text, width, INSPECTOR_BODY_INDENT, style)
}

/// Prose wrapped beside a `│` spine at `indent`. Blank source lines keep
/// the spine so a multi-paragraph body reads as one block.
pub fn spine_text(text: &str, width: usize, indent: &str, style: Style) -> Vec<Line<'static>> {
    let text = view::sanitize_cells(text);
    let prefix = format!("{indent}│");
    let text_prefix = format!("{prefix} ");
    let body_width = width.saturating_sub(view::cell_width(&text_prefix)).max(1);
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
    super::layout::fit_lines(lines, width)
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
        .map(|(label, _)| view::cell_width(label))
        .max()
        .unwrap_or(0)
        .min(INSPECTOR_LABEL_CAP);
    rows.into_iter()
        .map(|(label, value)| {
            // A label past the cap is cut like any other cell, so one long
            // key cannot push every value out of its column.
            let label = view::truncate_line(label, label_width);
            let pad = " ".repeat(label_width.saturating_sub(view::cell_width(&label)));
            let prefix = format!("{INSPECTOR_BODY_INDENT}{label}{pad}  ");
            super::layout::fit(
                Line::from(vec![
                    Span::styled(prefix.clone(), theme().dim),
                    Span::raw(view::truncate_line(
                        &value,
                        width.saturating_sub(view::cell_width(&prefix)),
                    )),
                ]),
                width,
            )
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
        let lines = if self.language == "text" {
            self.text
                .lines()
                .map(|line| {
                    Line::from(Span::styled(
                        format!(
                            "{}{}",
                            self.indent,
                            view::truncate_line(
                                line,
                                self.width.saturating_sub(view::cell_width(self.indent))
                            )
                        ),
                        self.plain_style,
                    ))
                })
                .collect()
        } else {
            view::highlighted_code_lines(self.text, self.language, self.width, self.indent)
        };
        super::layout::fit_lines(lines, self.width)
    }
}
