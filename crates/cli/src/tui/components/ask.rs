//! Standard interactive clarification form used by the `ask` tool.
//!
//! Topics are the navigation unit (Tab / Shift+Tab). Each topic contains
//! agent-authored, explained multiple-choice questions plus an always-present
//! optional `Other` input for an answer outside those choices.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::{Line, Span};

use orca_harness_tools::{AskAnswer, AskRequest, AskResponse, AskTopic, AskTopicAnswer};

use crate::view::glyphs::glyphs;
use crate::view::{self, theme};

#[derive(Debug, Clone, Default)]
struct QuestionDraft {
    values: Vec<String>,
    option: usize,
}

#[derive(Debug, Clone, Default)]
struct TopicDraft {
    questions: Vec<QuestionDraft>,
    additional_context: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskFormEvent {
    Continue,
    Submit,
    Cancel,
}

/// Stateful clarification component. It owns the host request until the
/// caller submits/cancels it, so dropping the component safely dismisses the
/// pending tool call.
pub struct AskForm {
    request: AskRequest,
    topic: usize,
    focus: usize,
    drafts: Vec<TopicDraft>,
}

impl AskForm {
    pub fn new(request: AskRequest) -> Self {
        let drafts = request
            .topics
            .iter()
            .map(|topic| TopicDraft {
                questions: vec![QuestionDraft::default(); topic.questions.len()],
                additional_context: String::new(),
            })
            .collect();
        Self {
            request,
            topic: 0,
            focus: 0,
            drafts,
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if let Some(target) = self.text_target_mut() {
            target.push_str(&text);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> AskFormEvent {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return AskFormEvent::Cancel,
            KeyCode::Char('c') if ctrl => return AskFormEvent::Cancel,
            KeyCode::Tab if shift => self.move_topic(-1),
            KeyCode::BackTab => self.move_topic(-1),
            KeyCode::Tab => self.move_topic(1),
            KeyCode::Up => self.move_focus(-1),
            KeyCode::Down => self.move_focus(1),
            KeyCode::Left => self.move_option(-1),
            KeyCode::Right => self.move_option(1),
            KeyCode::Char(' ') if self.focused_question_has_options() => self.choose_option(),
            KeyCode::Enter => return AskFormEvent::Submit,
            KeyCode::Backspace => {
                if let Some(target) = self.text_target_mut() {
                    target.pop();
                }
            }
            KeyCode::Char(c) if !ctrl => {
                if let Some(target) = self.text_target_mut() {
                    target.push(c);
                }
            }
            _ => {}
        }
        AskFormEvent::Continue
    }

    pub fn finish(self, event: AskFormEvent) {
        let response = match event {
            AskFormEvent::Submit => AskResponse::Answered(self.answers()),
            AskFormEvent::Cancel | AskFormEvent::Continue => AskResponse::Cancelled,
        };
        let _ = self.request.respond.send(response);
    }

    fn answers(&self) -> Vec<AskTopicAnswer> {
        self.request
            .topics
            .iter()
            .zip(&self.drafts)
            .map(|(topic, draft)| AskTopicAnswer {
                id: topic.id.clone(),
                answers: topic
                    .questions
                    .iter()
                    .zip(&draft.questions)
                    .map(|(question, answer)| AskAnswer {
                        question_id: question.id.clone(),
                        values: answer.values.clone(),
                    })
                    .collect(),
                additional_context: draft.additional_context.trim().to_string(),
            })
            .collect()
    }

    fn current_topic(&self) -> &AskTopic {
        &self.request.topics[self.topic]
    }

    fn move_topic(&mut self, delta: isize) {
        let len = self.request.topics.len() as isize;
        self.topic = (self.topic as isize + delta).rem_euclid(len) as usize;
        self.focus = 0;
    }

    fn move_focus(&mut self, delta: isize) {
        let len = self.current_topic().questions.len() + 1;
        self.focus = (self.focus as isize + delta).rem_euclid(len as isize) as usize;
    }

    fn focused_question_has_options(&self) -> bool {
        self.current_topic()
            .questions
            .get(self.focus)
            .is_some_and(|question| !question.options.is_empty())
    }

    fn move_option(&mut self, delta: isize) {
        let Some(question) = self.current_topic().questions.get(self.focus) else {
            return;
        };
        if question.options.is_empty() {
            return;
        }
        let len = question.options.len() as isize;
        let draft = &mut self.drafts[self.topic].questions[self.focus];
        draft.option = (draft.option as isize + delta).rem_euclid(len) as usize;
    }

    fn choose_option(&mut self) {
        let question = &self.request.topics[self.topic].questions[self.focus];
        let draft = &mut self.drafts[self.topic].questions[self.focus];
        let label = question.options[draft.option].label.clone();
        if question.multiple {
            if let Some(index) = draft.values.iter().position(|value| value == &label) {
                draft.values.remove(index);
            } else {
                draft.values.push(label);
            }
        } else {
            draft.values = vec![label];
        }
    }

    fn text_target_mut(&mut self) -> Option<&mut String> {
        let question_count = self.request.topics[self.topic].questions.len();
        (self.focus == question_count).then(|| &mut self.drafts[self.topic].additional_context)
    }

    fn topic_complete_at(&self, index: usize) -> bool {
        self.drafts[index]
            .questions
            .iter()
            .all(|answer| answer.values.iter().any(|value| !value.trim().is_empty()))
    }

    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let t = theme();
        let g = glyphs();
        let mut lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(format!("  {} ", g.attention), t.accent),
                Span::styled("Clarification needed", t.strong),
                Span::styled(
                    format!(
                        " · topic {} of {}",
                        self.topic + 1,
                        self.request.topics.len()
                    ),
                    t.dim,
                ),
            ]),
        ];

        let mut topic_line = vec![Span::raw("    ")];
        for (index, topic) in self.request.topics.iter().enumerate() {
            if index > 0 {
                topic_line.push(Span::styled("  ───  ", t.dim));
            }
            let glyph = if self.topic_complete_at(index) {
                g.done
            } else {
                g.waiting
            };
            let style = if index == self.topic { t.select } else { t.dim };
            topic_line.push(Span::styled(format!("{glyph} {}", topic.title), style));
        }
        lines.push(Line::from(topic_line));
        lines.push(Line::from(""));

        let topic = self.current_topic();
        let draft = &self.drafts[self.topic];
        for (question_index, (question, answer)) in
            topic.questions.iter().zip(&draft.questions).enumerate()
        {
            let active = question_index == self.focus;
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {} ", if active { g.cursor } else { " " }),
                    if active { t.accent } else { t.dim },
                ),
                Span::styled(
                    view::truncate_line(&question.question, width.saturating_sub(6)),
                    if active { t.strong } else { t.dim },
                ),
            ]));
            if question.options.is_empty() {
                let value = answer.values.first().map(String::as_str).unwrap_or("");
                let shown = if value.is_empty() {
                    "Type your answer…"
                } else {
                    value
                };
                lines.push(Line::from(vec![
                    Span::styled("      │ ", if active { t.accent } else { t.dim }),
                    Span::styled(
                        view::truncate_line(shown, width.saturating_sub(9)),
                        if value.is_empty() {
                            t.dim
                        } else {
                            ratatui::style::Style::default()
                        },
                    ),
                ]));
            } else {
                for (option_index, option) in question.options.iter().enumerate() {
                    let selected = answer.values.iter().any(|value| value == &option.label);
                    let cursor = active && answer.option == option_index;
                    let marker = if selected { g.done } else { g.waiting };
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("      {} ", if cursor { g.cursor } else { " " }),
                            if cursor { t.accent } else { t.dim },
                        ),
                        Span::styled(
                            format!("{marker} {}", option.label),
                            if selected || cursor { t.strong } else { t.dim },
                        ),
                    ]));
                    if let Some(description) = &option.description {
                        // The explanation the agent wrote is the point of
                        // the option: wrap it rather than cut it.
                        let description = view::sanitize_cells(description);
                        for part in textwrap::wrap(&description, width.saturating_sub(12).max(16)) {
                            lines.push(Line::from(Span::styled(
                                format!("            {part}"),
                                t.dim,
                            )));
                        }
                    }
                }
            }
            lines.push(Line::from(""));
        }

        let active = self.focus == topic.questions.len();
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {} ", if active { g.cursor } else { " " }),
                if active { t.accent } else { t.dim },
            ),
            Span::styled("Other", if active { t.strong } else { t.dim }),
            Span::styled(
                view::truncate_line(
                    " · optional answer outside these choices",
                    width.saturating_sub(9),
                ),
                t.dim,
            ),
        ]));
        let additional = if draft.additional_context.is_empty() {
            "Type another answer or requirement…"
        } else {
            &draft.additional_context
        };
        lines.push(Line::from(vec![
            Span::styled("      │ ", if active { t.accent } else { t.dim }),
            Span::styled(
                view::truncate_line(additional, width.saturating_sub(9)),
                if draft.additional_context.is_empty() {
                    t.dim
                } else {
                    ratatui::style::Style::default()
                },
            ),
        ]));
        lines.push(Line::from(Span::styled(
            view::truncate_line(
                "    ↑↓ question · ←→ option · space choose · tab next topic · enter send · esc cancel",
                width,
            ),
            t.dim,
        )));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_harness_tools::{AskOption, AskQuestion};
    use tokio::sync::oneshot;

    fn form() -> AskForm {
        let (respond, _answer) = oneshot::channel();
        AskForm::new(AskRequest {
            call_id: "c1".into(),
            topics: vec![AskTopic {
                id: "scope".into(),
                title: "Scope".into(),
                questions: vec![AskQuestion {
                    id: "release".into(),
                    question: "Which release?".into(),
                    options: vec![
                        AskOption {
                            label: "Beta".into(),
                            description: Some("Ship quickly to a limited audience".into()),
                        },
                        AskOption {
                            label: "General availability".into(),
                            description: Some(
                                "Prepare for all users and production support".into(),
                            ),
                        },
                    ],
                    multiple: false,
                }],
            }],
            respond,
        })
    }

    #[test]
    fn enter_submits_even_when_no_option_is_selected() {
        let mut form = form();
        assert_eq!(
            form.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AskFormEvent::Submit
        );
    }

    #[test]
    fn renders_square_topic_and_default_other_input() {
        let text = form()
            .lines(100)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("□ Scope"));
        assert!(text.contains("Other · optional answer outside these choices"));
        assert!(text.contains("Type another answer"));
    }

    #[test]
    fn long_option_descriptions_wrap_instead_of_truncating() {
        let lines = form().lines(40);
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert!(lines.iter().all(|line| line.width() <= 40), "{text:?}");
        assert!(
            text.iter().any(|line| line.contains("production support")),
            "the end of the description is still there: {text:?}"
        );
    }
}
