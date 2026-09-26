//! Compact system notifications embedded in the transcript.

use ratatui::text::{Line, Span};

use crate::view::glyphs::glyphs;
use crate::view::theme;

#[derive(Clone, Copy)]
pub enum NotificationKind {
    Notice,
    Error,
}

pub struct Notification {
    kind: NotificationKind,
    text: String,
}

impl Notification {
    pub fn notice(text: impl Into<String>) -> Self {
        Self {
            kind: NotificationKind::Notice,
            text: text.into(),
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            kind: NotificationKind::Error,
            text: text.into(),
        }
    }

    pub fn line(self) -> Line<'static> {
        let t = theme();
        let body = match self.kind {
            NotificationKind::Notice => t.dim,
            NotificationKind::Error => t.error,
        };
        // A notice keeps the quiet bullet; an error takes the rails'
        // failure mark, so the mark alone says which one it is.
        let mark = match self.kind {
            NotificationKind::Notice => "•".to_string(),
            NotificationKind::Error => glyphs().failed.to_string(),
        };
        Line::from(vec![
            Span::styled(format!("{mark} "), body),
            Span::styled(
                match self.kind {
                    NotificationKind::Notice => self.text,
                    NotificationKind::Error => format!("error: {}", self.text),
                },
                body,
            ),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_and_error_are_told_apart_by_their_mark() {
        let notice = Notification::notice("connected").line();
        let error = Notification::error("failed").line();
        assert_eq!(notice.spans[0].content.as_ref(), "• ");
        assert_eq!(
            error.spans[0].content.as_ref(),
            format!("{} ", glyphs().failed)
        );
    }
}
