//! Standard transcript rail: one header plus tightly packed detail rows.

use ratatui::text::Line;

use super::transcript::{append_block, BlockSpacing};

pub struct Rail {
    header: Line<'static>,
    rows: Vec<Line<'static>>,
}

impl Rail {
    pub fn new(header: Line<'static>) -> Self {
        Self {
            header,
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

    pub fn append_to(self, transcript: &mut Vec<Line<'static>>) {
        let mut lines = Vec::with_capacity(self.rows.len() + 1);
        lines.push(self.header);
        lines.extend(self.rows);
        append_block(transcript, lines, BlockSpacing::Section, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rail_is_a_section_with_tight_internal_rows() {
        let mut transcript = vec![Line::from("prose")];
        let mut rail = Rail::new(Line::from("Thinking"));
        rail.push(Line::from("detail one"));
        rail.push(Line::from("detail two"));
        rail.append_to(&mut transcript);

        let text: Vec<String> = transcript
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(text, ["prose", "", "Thinking", "detail one", "detail two"]);
    }
}
