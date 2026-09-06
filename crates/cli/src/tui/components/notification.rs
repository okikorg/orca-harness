//! Compact system notifications embedded in the transcript.

use ratatui::text::{Line, Span};

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
        Line::from(vec![
            Span::styled("• ", body),
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
    fn notice_and_error_share_the_system_glyph() {
        for line in [
            Notification::notice("connected").line(),
            Notification::error("failed").line(),
        ] {
            assert_eq!(line.spans[0].content.as_ref(), "• ");
        }
    }
}
