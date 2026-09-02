use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use super::normalize_label;
use crate::view::{cell_width, truncate_line, Theme};

struct Participant {
    id: String,
    label: String,
}

#[derive(Clone, Copy)]
enum MessageEnd {
    Line,
    Arrow,
    Cross,
    Open,
}

impl MessageEnd {
    fn compact_glyph(self) -> &'static str {
        match self {
            Self::Line => "─",
            Self::Arrow => "▶",
            Self::Cross => "×",
            Self::Open => "▷",
        }
    }

    fn terminal_glyph(self, points_right: bool) -> char {
        match (self, points_right) {
            (Self::Line, _) => '│',
            (Self::Arrow, true) => '▶',
            (Self::Arrow, false) => '◀',
            (Self::Cross, _) => '×',
            (Self::Open, true) => '▷',
            (Self::Open, false) => '◁',
        }
    }
}

enum SequenceEvent {
    Message {
        from: usize,
        to: usize,
        label: String,
        dashed: bool,
        end: MessageEnd,
    },
    Annotation(String),
}

#[derive(Default)]
pub(super) struct Sequence {
    participants: Vec<Participant>,
    events: Vec<SequenceEvent>,
}

impl Sequence {
    pub(super) fn parse(source: &[&str]) -> Option<Self> {
        let mut sequence = Self::default();
        let mut blocks = Vec::new();
        for raw in source {
            let statement = raw.trim().trim_end_matches(';').trim();
            if statement.is_empty() || statement.starts_with("%%") {
                continue;
            }
            if let Some((id, label)) = parse_participant(statement) {
                sequence.upsert_participant(id, label);
                continue;
            }
            if let Some(message) = parse_message(statement) {
                let from = sequence.upsert_participant(message.from.clone(), message.from);
                let to = sequence.upsert_participant(message.to.clone(), message.to);
                sequence.events.push(SequenceEvent::Message {
                    from,
                    to,
                    label: normalize_label(message.label),
                    dashed: message.dashed,
                    end: message.end,
                });
                continue;
            }
            if is_control(statement) {
                let keyword = statement
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                match keyword.as_str() {
                    "rect" => blocks.push(ControlBlock::Style),
                    "loop" | "alt" | "opt" | "par" | "critical" | "break" => {
                        blocks.push(ControlBlock::Semantic);
                        sequence
                            .events
                            .push(SequenceEvent::Annotation(format!("┌─ {statement}")));
                    }
                    "end" if matches!(blocks.pop(), Some(ControlBlock::Style)) => {}
                    "end" => sequence
                        .events
                        .push(SequenceEvent::Annotation("└─".to_string())),
                    "else" | "and" => sequence
                        .events
                        .push(SequenceEvent::Annotation(format!("├─ {statement}"))),
                    "note" => sequence.events.push(SequenceEvent::Annotation(format!(
                        "◇ {}",
                        statement
                            .split_once(' ')
                            .map_or(statement, |(_, note)| note)
                    ))),
                    "autonumber" | "activate" | "deactivate" => {}
                    _ => sequence
                        .events
                        .push(SequenceEvent::Annotation(statement.to_string())),
                }
                continue;
            }
            return None;
        }
        (!sequence.participants.is_empty()).then_some(sequence)
    }

    fn upsert_participant(&mut self, id: String, label: String) -> usize {
        if let Some(index) = self
            .participants
            .iter()
            .position(|participant| participant.id == id)
        {
            if label != id {
                self.participants[index].label = label;
            }
            return index;
        }
        self.participants.push(Participant { id, label });
        self.participants.len() - 1
    }

    pub(super) fn render(&self, width: usize, indent: &str, theme: &Theme) -> Vec<Line<'static>> {
        let inner = width.saturating_sub(cell_width(indent));
        let column_width = inner / self.participants.len().max(1);
        if column_width >= 8 {
            self.render_lifelines(column_width, width, indent, theme)
        } else {
            self.render_compact(width, indent, theme)
        }
    }

    fn render_lifelines(
        &self,
        column_width: usize,
        width: usize,
        indent: &str,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let layout = SequenceLayout::new(self.participants.len(), column_width, indent, theme);
        let mut output = vec![Line::from(vec![
            Span::raw(indent.to_string()),
            Span::styled(
                self.participants
                    .iter()
                    .map(|participant| centered(&participant.label, column_width))
                    .collect::<String>(),
                theme.strong.add_modifier(Modifier::BOLD),
            ),
        ])];
        output.push(layout.lifelines());
        for event in &self.events {
            match event {
                SequenceEvent::Message {
                    from,
                    to,
                    label,
                    dashed,
                    end,
                } => {
                    if !label.is_empty() {
                        output.push(layout.label(label, *from, *to));
                    }
                    output.push(layout.message(*from, *to, *dashed, *end));
                }
                SequenceEvent::Annotation(annotation) => {
                    output.push(control_line(annotation, width, indent, theme.dim))
                }
            }
        }
        output.push(layout.lifelines());
        output
    }

    fn render_compact(&self, width: usize, indent: &str, theme: &Theme) -> Vec<Line<'static>> {
        let participant_names = self
            .participants
            .iter()
            .map(|participant| participant.label.as_str())
            .collect::<Vec<_>>()
            .join(" │ ");
        let mut output = vec![styled_line(
            &participant_names,
            width,
            indent,
            theme.strong.add_modifier(Modifier::BOLD),
        )];
        for event in &self.events {
            let text = match event {
                SequenceEvent::Message {
                    from,
                    to,
                    label,
                    dashed,
                    end,
                } => {
                    let line = if *dashed { "┄" } else { "─" };
                    let label = if label.is_empty() {
                        String::new()
                    } else {
                        format!(" {label} ")
                    };
                    format!(
                        "{} {line}{label}{line}{} {}",
                        self.participants[*from].label,
                        end.compact_glyph(),
                        self.participants[*to].label
                    )
                }
                SequenceEvent::Annotation(annotation) => annotation.clone(),
            };
            output.push(if matches!(event, SequenceEvent::Annotation(_)) {
                control_line(&text, width, indent, theme.dim)
            } else {
                styled_line(&text, width, indent, theme.accent)
            });
        }
        output
    }
}

enum ControlBlock {
    Semantic,
    Style,
}

struct ParsedMessage<'a> {
    from: String,
    to: String,
    label: &'a str,
    dashed: bool,
    end: MessageEnd,
}

fn parse_message(statement: &str) -> Option<ParsedMessage<'_>> {
    let (route, label) = statement.split_once(':').unwrap_or((statement, ""));
    for (marker, dashed, end) in [
        ("-->>", true, MessageEnd::Arrow),
        ("--)", true, MessageEnd::Open),
        ("--x", true, MessageEnd::Cross),
        ("-->", true, MessageEnd::Line),
        ("->>", false, MessageEnd::Arrow),
        ("-)", false, MessageEnd::Open),
        ("-x", false, MessageEnd::Cross),
        ("->", false, MessageEnd::Line),
    ] {
        let Some(position) = route.find(marker) else {
            continue;
        };
        let from = route[..position].trim();
        let to = route[position + marker.len()..].trim();
        if !is_identifier(from) || !is_identifier(to) {
            return None;
        }
        return Some(ParsedMessage {
            from: from.to_string(),
            to: to.to_string(),
            label,
            dashed,
            end,
        });
    }
    None
}

fn parse_participant(statement: &str) -> Option<(String, String)> {
    let body = statement
        .strip_prefix("participant ")
        .or_else(|| statement.strip_prefix("actor "))?;
    let (id, label) = body
        .split_once(" as ")
        .map_or((body, body), |(id, label)| (id, label));
    let id = id.trim().to_string();
    is_identifier(&id).then(|| (id.clone(), normalize_label(label)))
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '-' | '.'))
}

fn is_control(statement: &str) -> bool {
    let keyword = statement
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        keyword.as_str(),
        "note"
            | "loop"
            | "alt"
            | "else"
            | "end"
            | "opt"
            | "par"
            | "and"
            | "critical"
            | "break"
            | "rect"
            | "autonumber"
            | "activate"
            | "deactivate"
    )
}

struct SequenceLayout<'a> {
    used: usize,
    positions: Vec<usize>,
    indent: &'a str,
    theme: &'a Theme,
}

impl<'a> SequenceLayout<'a> {
    fn new(
        participant_count: usize,
        column_width: usize,
        indent: &'a str,
        theme: &'a Theme,
    ) -> Self {
        Self {
            used: column_width * participant_count,
            positions: (0..participant_count)
                .map(|index| index * column_width + column_width / 2)
                .collect(),
            indent,
            theme,
        }
    }

    fn lifelines(&self) -> Line<'static> {
        let mut cells = vec![' '; self.used];
        for position in &self.positions {
            cells[*position] = '│';
        }
        self.line(cells, self.theme.dim)
    }

    fn label(&self, label: &str, from: usize, to: usize) -> Line<'static> {
        let start = self.positions[from];
        let end = self.positions[to];
        let distance = start.abs_diff(end).max(4);
        let label = truncate_line(label, distance.saturating_sub(2).max(1));
        let midpoint = (start + end) / 2;
        let left = midpoint.saturating_sub(cell_width(&label) / 2);
        let right = self.used.saturating_sub(left + cell_width(&label));
        Line::from(vec![
            Span::raw(self.indent.to_string()),
            Span::raw(" ".repeat(left)),
            Span::styled(label, self.theme.code.remove_modifier(Modifier::ITALIC)),
            Span::raw(" ".repeat(right)),
        ])
    }

    fn message(&self, from: usize, to: usize, dashed: bool, end: MessageEnd) -> Line<'static> {
        let start = self.positions[from];
        let finish = self.positions[to];
        let mut cells = vec![' '; self.used];
        for position in &self.positions {
            cells[*position] = '│';
        }
        if start == finish {
            if start + 2 < self.used {
                cells[start] = '├';
                cells[start + 1] = if dashed { '┄' } else { '─' };
                cells[start + 2] = match end {
                    MessageEnd::Cross => '×',
                    MessageEnd::Line => '┐',
                    MessageEnd::Arrow | MessageEnd::Open => '↩',
                };
            } else {
                cells[start] = '↻';
            }
            return self.line(cells, self.theme.accent);
        }

        let (left, right) = if start < finish {
            (start, finish)
        } else {
            (finish, start)
        };
        for cell in &mut cells[left + 1..right] {
            *cell = if dashed { '┄' } else { '─' };
        }
        cells[start] = if start < finish { '├' } else { '┤' };
        cells[finish] = end.terminal_glyph(start < finish);
        self.line(cells, self.theme.accent)
    }

    fn line(&self, cells: Vec<char>, style: Style) -> Line<'static> {
        Line::from(vec![
            Span::raw(self.indent.to_string()),
            Span::styled(cells.into_iter().collect::<String>(), style),
        ])
    }
}

fn centered(text: &str, width: usize) -> String {
    let text = truncate_line(text, width.saturating_sub(1));
    let padding = width.saturating_sub(cell_width(&text));
    format!(
        "{}{}{}",
        " ".repeat(padding / 2),
        text,
        " ".repeat(padding - padding / 2)
    )
}

fn styled_line(text: &str, width: usize, indent: &str, style: Style) -> Line<'static> {
    let available = width.saturating_sub(cell_width(indent));
    Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled(truncate_line(text, available), style),
    ])
}

fn control_line(text: &str, width: usize, indent: &str, style: Style) -> Line<'static> {
    let available = width.saturating_sub(cell_width(indent));
    let mut text = truncate_line(text, available);
    if text.starts_with(['┌', '├', '└']) {
        text.push_str(&"─".repeat(available.saturating_sub(cell_width(&text))));
    }
    Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled(text, style),
    ])
}
