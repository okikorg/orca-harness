//! Responsive empty-session welcome card.

use ratatui::text::{Line, Span};

use crate::view::{self, theme};

pub struct Welcome<'a> {
    pub version: &'a str,
    pub model: &'a str,
    pub workspace: &'a str,
    /// The selected reasoning effort, shown beside the model.
    pub effort: Option<&'a str>,
    /// The checkout's branch, shown beside the workspace.
    pub branch: Option<&'a str>,
    /// The newest recorded sessions for this workspace, newest first.
    pub recent: &'a [RecentSession],
}

/// One recorded session, as the welcome card lists it.
#[derive(Clone, Debug)]
pub struct RecentSession {
    pub id: String,
    pub model: String,
    /// `2h ago`, from the session's creation time.
    pub age: String,
}

impl Welcome<'_> {
    /// The card, centred in the terminal and clipped to the transcript area.
    pub fn lines(&self, full_height: usize, clip: usize, width: usize) -> Vec<Line<'static>> {
        let t = theme();
        let available_width = width.saturating_sub(4).min(64);
        let value_width = available_width.saturating_sub(11);
        // A value with a dim note after it: the effort beside the model,
        // the branch beside the workspace.
        let noted = |label: &'static str, value: &str, note: Option<String>| {
            let note = note.map(|note| format!("  {note}")).unwrap_or_default();
            let value_width = value_width.saturating_sub(view::cell_width(&note));
            Line::from(vec![
                Span::styled(format!("{label:<11}"), t.dim),
                Span::raw(view::truncate_line(value, value_width)),
                Span::styled(note, t.dim),
            ])
        };
        let command = |name: &'static str, action: &'static str| {
            Line::from(vec![
                Span::styled(format!("{name:<11}"), t.key),
                Span::styled(action, t.dim),
            ])
        };
        let g = crate::view::glyphs::glyphs();
        let branch = self.branch.map(|branch| {
            if g.branch.is_empty() {
                branch.to_string()
            } else {
                format!("{} {branch}", g.branch)
            }
        });
        // Three groups with air between them: who this is, where it is
        // running, and what to do next.
        let mut content = vec![
            Line::from(vec![
                Span::styled("▀▄ ", t.accent),
                Span::styled("ORCACODE", t.strong),
                Span::styled(format!("  v{}", self.version), t.dim),
            ]),
            Line::from(Span::styled(
                "A small, fast agent runtime for your terminal",
                t.dim,
            )),
            Line::from(""),
            noted("model", self.model, self.effort.map(str::to_string)),
            noted("workspace", self.workspace, branch),
        ];
        for (index, session) in self.recent.iter().enumerate() {
            let label = if index == 0 { "recent" } else { "" };
            let short: String = session.id.chars().take(8).collect();
            content.push(Line::from(vec![
                Span::styled(format!("{label:<11}"), t.dim),
                Span::raw(format!("{:<10}", session.age)),
                Span::styled(
                    view::truncate_line(
                        &format!("{}  {short}", session.model),
                        value_width.saturating_sub(10),
                    ),
                    t.dim,
                ),
            ]));
        }
        content.extend([
            Line::from(""),
            Line::from(vec![
                Span::styled("› ", t.accent),
                Span::styled("Describe a task to begin", t.strong),
            ]),
            command("/help", "commands"),
            command("/models", "switch model"),
            command("/mode", "plan, orchestrate, auto, yolo"),
        ]);
        if !self.recent.is_empty() {
            content.push(command("/sessions", "resume a session"));
        }
        let content = super::layout::fit_lines(content, available_width);
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
            .take(clip)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_names_effort_branch_and_recent_sessions_when_it_has_them() {
        let recent = [RecentSession {
            id: "3f2a91c0-1234".into(),
            model: "claude-sonnet-5".into(),
            age: "2h ago".into(),
        }];
        let lines = Welcome {
            version: "0.7.0",
            model: "claude-sonnet-5",
            workspace: "orca-harness",
            effort: Some("high"),
            branch: Some("main"),
            recent: &recent,
        }
        .lines(40, 40, 90);
        let text: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        let joined = text.join("\n");
        assert!(joined.contains("claude-sonnet-5  high"), "{joined}");
        assert!(joined.contains("main"), "{joined}");
        assert!(
            crate::tui::text::has_row(
                &joined,
                &["recent", "2h ago", "claude-sonnet-5", "3f2a91c0"]
            ),
            "{joined}"
        );
        assert!(joined.contains("/sessions"), "{joined}");
        assert!(lines.iter().all(|line| line.width() <= 90));
    }
}
