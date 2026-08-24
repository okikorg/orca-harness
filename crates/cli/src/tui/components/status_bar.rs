//! Standard single-row application status bar.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::view;

pub struct StatusBar<'a> {
    pub model: &'a str,
    pub state: &'a str,
    pub mode: &'a str,
    pub context: &'a str,
    pub stats: &'a str,
    pub todo: &'a str,
    pub queue: &'a str,
    pub hint: &'a str,
    pub workspace: &'a str,
}

impl StatusBar<'_> {
    pub fn line(&self, width: usize, style: Style) -> Line<'static> {
        let status = format!(
            " {} · {}{} · {}{}{}{} · {} · {}",
            self.model,
            self.state,
            self.mode,
            self.context,
            self.stats,
            self.todo,
            self.queue,
            self.hint,
            self.workspace,
        );
        Line::from(Span::styled(view::truncate_line(&status, width), style))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembles_segments_in_the_standard_order() {
        let line = StatusBar {
            model: "model",
            state: "idle",
            mode: " · normal",
            context: "ctx 10%",
            stats: "",
            todo: "",
            queue: "",
            hint: "enter send",
            workspace: "repo",
        }
        .line(100, Style::default());
        assert_eq!(
            line.spans[0].content.as_ref(),
            " model · idle · normal · ctx 10% · enter send · repo"
        );
    }
}
