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
    let g = glyphs();
    let mut header = vec![Span::styled(format!("  {label}"), t.strong)];
    // A meter where the style draws one, the plain count otherwise.
    match g.meter_bar(16, done as f64 / items.len() as f64) {
        Some(bar) => {
            let filled = bar
                .chars()
                .take_while(|&c| Some(c) == g.meter.map(|m| m.0))
                .count();
            let used: String = bar.chars().take(filled).collect();
            let free: String = bar.chars().skip(filled).collect();
            header.push(Span::raw("  "));
            header.push(Span::styled(used, t.accent));
            header.push(Span::styled(free, t.dim));
            header.push(Span::styled(format!(" {done}/{}", items.len()), t.dim));
        }
        None => header.push(Span::styled(
            format!(" · {done}/{} done", items.len()),
            t.dim,
        )),
    }
    let mut lines = vec![Line::from(header)];
    let last = items.len() - 1;
    for (index, item) in items.iter().enumerate() {
        let branch = TreeBranch {
            indent: "  ",
            last: index == last,
        };
        // Hollow is work, solid is done: the active item is the same
        // hollow mark as a pending one, told apart by colour and weight.
        let (marker, marker_style, style) = match item.state {
            ProgressState::Completed => (
                g.done,
                t.success,
                t.dim.add_modifier(ratatui::style::Modifier::CROSSED_OUT),
            ),
            ProgressState::Active => (g.running_frame(0), t.accent, t.strong),
            ProgressState::Pending => (g.waiting, t.dim, t.dim),
        };
        let prefix = branch.prefix();
        let content = view::truncate_line(
            item.content,
            width.saturating_sub(view::cell_width(&prefix) + 2),
        );
        lines.push(Line::from(vec![
            Span::styled(prefix, t.dim),
            Span::styled(format!("{marker} "), marker_style),
            Span::styled(content, style),
        ]));
    }
    super::layout::fit_lines(lines, width)
}
