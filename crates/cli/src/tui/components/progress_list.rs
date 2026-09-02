//! Compact tree-shaped progress lists.

use ratatui::text::{Line, Span};

use super::tree::TreeBranch;
use crate::view::glyphs::glyphs;
use crate::view::{self, theme};

#[derive(Clone, Copy)]
pub enum ProgressState {
    Completed,
    Active,
    Pending,
}

pub struct ProgressItem<'a> {
    pub content: &'a str,
    pub state: ProgressState,
}

pub fn progress_list(label: &str, items: &[ProgressItem<'_>], width: usize) -> Vec<Line<'static>> {
    if items.is_empty() {
        return Vec::new();
    }
    let t = theme();
    let done = items
        .iter()
        .filter(|item| matches!(item.state, ProgressState::Completed))
        .count();
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("  {label}"), t.strong),
        Span::styled(format!(" · {done}/{} done", items.len()), t.dim),
    ])];
    let last = items.len() - 1;
    for (index, item) in items.iter().enumerate() {
        let branch = TreeBranch {
            indent: "  ",
            last: index == last,
        };
        let g = glyphs();
        let (marker, style) = match item.state {
            ProgressState::Completed => (g.done.to_string(), t.dim),
            ProgressState::Active => (g.cursor.to_string(), t.strong),
            ProgressState::Pending => (g.waiting.to_string(), t.dim),
        };
        let prefix = format!("{}{marker} ", branch.prefix());
        let content = view::truncate_line(
            item.content,
            width.saturating_sub(view::cell_width(&prefix)),
        );
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(content, style),
        ]));
    }
    lines
}
