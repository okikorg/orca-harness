//! Standard single-row application status bar.
//!
//! Segments carry a priority. When the row does not fit, the lowest
//! priority leaves first, so a narrow terminal keeps the action-required
//! state, the mode, the context gauge and the key hint, and gives up the
//! reasoning effort, the background stats and the counts. The workspace is anchored
//! at the right edge. Only when nothing droppable is left does the row
//! truncate.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::view;

/// Never dropped: an action-required state or non-normal mode.
pub const KEEP: u8 = u8::MAX;
pub const CONTEXT: u8 = 8;
pub const HINT: u8 = 7;
pub const MODEL: u8 = 6;
pub const WORKSPACE: u8 = 5;
/// Todo progress and the queue length.
pub const COUNTS: u8 = 4;
/// Background processes, kernels, agents.
pub const STATS: u8 = 3;
pub const EFFORT: u8 = 2;

const SEPARATOR: &str = " · ";
/// Cells between the last segment and a right-anchored trailing segment.
const TRAILING_GAP: usize = 2;

/// One piece of the bar. Most segments render in the bar's own style:
/// presence is the signal. A few carry their own spans where paint adds
/// information a glance needs: the mode chip, the context meter's level,
/// and bold keys in the hint.
#[derive(Clone, Debug)]
pub struct Segment {
    text: String,
    /// A shorter rendering the bar falls back to before dropping anything.
    compact: Option<String>,
    priority: u8,
    style: Option<Style>,
    /// Styled spans whose text is `text`; drawn in place of it.
    rich: Option<Vec<Span<'static>>>,
    /// Styled spans for `compact`.
    compact_rich: Option<Vec<Span<'static>>>,
}

impl Segment {
    pub fn new(text: impl Into<String>, priority: u8) -> Self {
        Self {
            text: text.into(),
            compact: None,
            priority,
            style: None,
            rich: None,
            compact_rich: None,
        }
    }

    /// A segment drawn from styled spans; its text is theirs.
    pub fn spans(spans: Vec<Span<'static>>, priority: u8) -> Self {
        let mut segment = Self::new(plain(&spans), priority);
        segment.rich = Some(spans);
        segment
    }

    /// A styled compact form, taken like [`Self::with_compact`].
    pub fn with_compact_spans(mut self, spans: Vec<Span<'static>>) -> Self {
        let compact = plain(&spans);
        if compact != self.text {
            self.compact = Some(compact);
            self.compact_rich = Some(spans);
        }
        self
    }

    /// A shorter form to show when the row is tight. Decoration gives way
    /// before information: every compact form is taken before the first
    /// segment is dropped.
    pub fn with_compact(mut self, compact: impl Into<String>) -> Self {
        let compact = compact.into();
        if compact != self.text {
            self.compact = Some(compact);
            self.compact_rich = None;
        }
        self
    }

    /// Switch to the compact form, spans and all.
    fn take_compact(&mut self) {
        if let Some(compact) = self.compact.take() {
            self.text = compact;
            self.rich = self.compact_rich.take();
        }
    }

    fn rendered(&self, style: Style) -> Vec<Span<'static>> {
        match &self.rich {
            Some(spans) => spans.clone(),
            None => vec![Span::styled(self.text.clone(), self.style.unwrap_or(style))],
        }
    }

    pub fn with_style(mut self, style: Style) -> Self {
        self.style = Some(style);
        self
    }

    fn width(&self) -> usize {
        view::cell_width(&self.text)
    }
}

#[derive(Default)]
pub struct StatusBar {
    segments: Vec<Segment>,
    trailing: Option<Segment>,
}

impl StatusBar {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a segment; empty text is skipped so callers can pass optional
    /// segments without branching.
    pub fn push(&mut self, segment: Segment) -> &mut Self {
        if !segment.text.is_empty() {
            self.segments.push(segment);
        }
        self
    }

    /// The segment pinned to the right edge.
    pub fn trailing(&mut self, segment: Segment) -> &mut Self {
        if !segment.text.is_empty() {
            self.trailing = Some(segment);
        }
        self
    }

    pub fn line(&self, width: usize, style: Style) -> Line<'static> {
        super::layout::fit(self.layout_line(width, style), width)
    }

    fn layout_line(&self, width: usize, style: Style) -> Line<'static> {
        let mut kept: Vec<Segment> = self.segments.clone();
        let mut trailing: Option<Segment> = self.trailing.clone();
        loop {
            let body = 1
                + kept.iter().map(|segment| segment.width()).sum::<usize>()
                + view::cell_width(SEPARATOR) * kept.len().saturating_sub(1);
            let total = body
                + trailing
                    .as_ref()
                    .map_or(0, |segment| TRAILING_GAP + segment.width());
            if total <= width {
                return self.assemble(&kept, trailing.as_ref(), width.saturating_sub(body), style);
            }
            // The workspace gives way first: its folder name is the most
            // recoverable thing on the row.
            if let Some(segment) = trailing.as_mut().filter(|s| s.compact.is_some()) {
                segment.take_compact();
                continue;
            }
            // Compact forms first, in bar order, so a tight row loses its
            // decoration (the context meter) before its key hints, and
            // both before any segment.
            if let Some(segment) = kept.iter_mut().find(|segment| segment.compact.is_some()) {
                segment.take_compact();
                continue;
            }
            // Drop the lowest priority; on a tie, the rightmost goes first.
            let lowest = kept
                .iter()
                .enumerate()
                .rev()
                .min_by_key(|(_, segment)| segment.priority)
                .map(|(index, segment)| (segment.priority, Some(index)));
            let trailing_priority = trailing.as_ref().map(|segment| (segment.priority, None));
            match [lowest, trailing_priority]
                .into_iter()
                .flatten()
                .min_by_key(|(priority, _)| *priority)
            {
                Some((priority, _)) if priority == KEEP => {
                    return self.assemble(&kept, None, 0, style);
                }
                Some((_, Some(index))) => {
                    kept.remove(index);
                }
                Some((_, None)) => trailing = None,
                None => return self.assemble(&kept, None, 0, style),
            }
        }
    }

    fn assemble(
        &self,
        kept: &[Segment],
        trailing: Option<&Segment>,
        slack: usize,
        style: Style,
    ) -> Line<'static> {
        let mut spans = vec![Span::styled(" ", style)];
        for (index, segment) in kept.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(SEPARATOR, style));
            }
            spans.extend(segment.rendered(style));
        }
        if let Some(segment) = trailing {
            let pad = slack.saturating_sub(segment.width()).max(TRAILING_GAP);
            spans.push(Span::styled(" ".repeat(pad), style));
            spans.extend(segment.rendered(style));
        }
        Line::from(spans)
    }
}

fn plain(spans: &[Span<'_>]) -> String {
    spans.iter().map(|span| span.content.as_ref()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn bar() -> StatusBar {
        let mut bar = StatusBar::new();
        bar.push(Segment::new("model", MODEL))
            .push(Segment::new("effort high", EFFORT))
            .push(Segment::new("idle", KEEP))
            .push(Segment::new("plan mode", KEEP))
            .push(Segment::new("ctx 10%", CONTEXT))
            .push(Segment::new("", STATS))
            .push(Segment::new("todo 1/3", COUNTS))
            .push(Segment::new("enter send", HINT))
            .trailing(Segment::new("repo", WORKSPACE));
        bar
    }

    #[test]
    fn compact_forms_give_way_before_any_segment_is_dropped() {
        let mut bar = StatusBar::new();
        bar.push(Segment::new("idle", KEEP))
            .push(Segment::new("ctx [==      ] 30%", CONTEXT).with_compact("ctx 30%"))
            .push(Segment::new("effort high", EFFORT));
        let wide = text(&bar.line(60, Style::default()));
        assert!(wide.contains("ctx [==      ] 30% · effort high"), "{wide}");
        let tight = text(&bar.line(30, Style::default()));
        assert_eq!(tight.trim_end(), " idle · ctx 30% · effort high");
        let tighter = text(&bar.line(16, Style::default()));
        assert_eq!(tighter.trim_end(), " idle · ctx 30%");
    }

    /// The trailing segment compacts before anything in the body does:
    /// the workspace name is the most recoverable thing on the row, and
    /// giving it up early buys back the context meter and a key hint.
    #[test]
    fn the_trailing_segment_compacts_before_the_body_does() {
        let mut bar = StatusBar::new();
        bar.push(Segment::new("idle", KEEP))
            .push(Segment::new("ctx [==      ] 30%", CONTEXT).with_compact("ctx 30%"))
            .trailing(Segment::new("orca (main)", WORKSPACE).with_compact("main"));
        let wide = text(&bar.line(40, Style::default()));
        assert!(wide.contains("ctx [==      ] 30%"), "{wide}");
        assert!(wide.ends_with("orca (main)"), "{wide}");
        // Tight enough to lose the workspace name, not the meter.
        let tight = text(&bar.line(32, Style::default()));
        assert!(tight.contains("ctx [==      ] 30%"), "{tight}");
        assert!(tight.ends_with("main"), "{tight}");
        // Tighter still: now the meter goes, and the branch outlives it.
        let tighter = text(&bar.line(22, Style::default()));
        assert_eq!(tighter.trim_end(), " idle · ctx 30%   main");
    }

    #[test]
    fn assembles_segments_in_order_and_anchors_the_workspace_right() {
        let line = bar().line(80, Style::default());
        let text = text(&line);
        assert!(
            text.starts_with(
                " model · effort high · idle · plan mode · ctx 10% · todo 1/3 · enter send"
            ),
            "{text}"
        );
        assert!(text.ends_with("repo"), "{text}");
        assert_eq!(line.width(), 80);
    }

    #[test]
    fn narrow_rows_drop_the_lowest_priority_first() {
        let text = text(&bar().line(66, Style::default()));
        assert!(!text.contains("effort"), "effort goes first: {text}");
        assert!(text.contains("todo 1/3"), "{text}");
        let text = super::tests::text(&bar().line(44, Style::default()));
        assert!(!text.contains("todo"), "{text}");
        assert!(text.contains("enter send"), "hint outlives counts: {text}");
        assert!(text.contains("plan mode"), "{text}");
    }

    #[test]
    fn keep_segments_survive_everything_else() {
        let line = bar().line(12, Style::default());
        assert_eq!(text(&line), " idle · pla…");
    }

    /// A span segment keeps its paint, and its compact form swaps in
    /// its own spans.
    #[test]
    fn span_segments_keep_their_styles_through_compaction() {
        let bold = Style::default().add_modifier(ratatui::style::Modifier::BOLD);
        let mut bar = StatusBar::new();
        bar.push(Segment::new("idle", KEEP)).push(
            Segment::spans(vec![Span::styled("[==  ] 30%", bold)], CONTEXT)
                .with_compact_spans(vec![Span::styled("30%", bold)]),
        );
        let wide = bar.line(40, Style::default());
        assert!(wide
            .spans
            .iter()
            .any(|s| s.content == "[==  ] 30%" && s.style == bold));
        let tight = bar.line(12, Style::default());
        assert_eq!(text(&tight).trim_end(), " idle · 30%");
        assert!(tight
            .spans
            .iter()
            .any(|s| s.content == "30%" && s.style == bold));
    }

    /// A plain segment still renders in the bar's own style.
    #[test]
    fn yolo_segment_renders_like_any_other() {
        let mut bar = StatusBar::new();
        bar.push(Segment::new("idle", KEEP))
            .push(Segment::new("yolo", KEEP));
        let style = Style::default().fg(ratatui::style::Color::Red);
        let line = bar.line(40, style);
        assert!(line.spans.iter().all(|span| span.style == style));
        assert_eq!(text(&line), " idle · yolo");
    }

    #[test]
    fn selected_reasoning_effort_follows_the_model() {
        let mut bar = StatusBar::new();
        bar.push(Segment::new("gpt-5", MODEL))
            .push(Segment::new("effort high", EFFORT))
            .push(Segment::new("idle", KEEP));
        assert_eq!(
            text(&bar.line(60, Style::default())),
            " gpt-5 · effort high · idle"
        );
    }
}
