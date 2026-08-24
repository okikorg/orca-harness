//! Atomic Inspector sections and source previews.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::view;

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
