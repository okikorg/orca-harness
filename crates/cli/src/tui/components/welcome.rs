//! Responsive empty-session welcome card.

use ratatui::text::{Line, Span};

use crate::view::{self, theme};

pub struct Welcome<'a> {
    pub version: &'a str,
    pub model: &'a str,
    pub workspace: &'a str,
}

impl Welcome<'_> {
    /// The card, centred in the terminal and clipped to the transcript area.
    pub fn lines(&self, full_height: usize, clip: usize, width: usize) -> Vec<Line<'static>> {
        let t = theme();
        let available_width = width.saturating_sub(4).min(64);
        let value_width = available_width.saturating_sub(11);
        let row = |label: &'static str, value: &str| {
            Line::from(vec![
                Span::styled(format!("{label:<11}"), t.dim),
                Span::raw(view::truncate_line(value, value_width)),
            ])
        };
        let content = vec![
            Line::from(vec![
                Span::styled("▀▄ ", t.accent),
                Span::styled("ORCACODE", t.strong),
                Span::styled(format!("  v{}", self.version), t.dim),
            ]),
            Line::from(Span::styled(
                "A small, fast agent runtime for your terminal",
                t.dim,
            )),
            row("model", self.model),
            row("workspace", self.workspace),
            Line::from(vec![
                Span::styled("› ", t.accent),
                Span::styled("Describe a task to begin", t.strong),
            ]),
            row("/help", "commands"),
            row("/models", "switch model"),
            row("/mode", "plan, auto review, yolo"),
        ];
        let content_width = content.iter().map(Line::width).max().unwrap_or(0);
        let indent = " ".repeat(width.saturating_sub(content_width) / 2);
        let content = content.into_iter().map(|line| {
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            spans.push(Span::raw(indent.clone()));
            spans.extend(line.spans);
            Line::from(spans)
        });
        let content: Vec<Line<'static>> = content.collect();
        let rows = content.len();
        let top = (full_height.saturating_sub(rows) / 2).min(clip.saturating_sub(rows));
        std::iter::repeat_n(Line::from(""), top)
            .chain(content)
            .collect()
    }
}
