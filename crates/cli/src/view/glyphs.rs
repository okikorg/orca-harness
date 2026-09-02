//! The mark vocabulary, chosen once per process by the `style` preference.
//!
//! Renderers never spell a state mark inline: they read the table so both
//! styles fill every slot and the marks stay one family. The Glyph style
//! uses same-size geometric squares for state, box-drawing strokes for
//! structure, and heavy/light rules for measure; the logo's `▀▄` stays
//! the only block element on screen.

#[cfg(not(test))]
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiStyle {
    Minimal,
    Glyph,
}

impl UiStyle {
    pub const ALL: [Self; 2] = [Self::Minimal, Self::Glyph];

    pub fn label(self) -> &'static str {
        match self {
            Self::Minimal => "Minimal",
            Self::Glyph => "Glyph",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Glyph => "glyph",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Minimal => "plain marks, text-only status",
            Self::Glyph => "square state marks, rails, context meter",
        }
    }

    pub fn from_slug(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|style| style.slug() == value)
    }

    /// The persisted choice, Minimal when none is saved.
    pub fn stored() -> Self {
        crate::config::stored_style()
            .as_deref()
            .and_then(Self::from_slug)
            .unwrap_or(Self::Minimal)
    }

    pub fn glyphs(self) -> &'static Glyphs {
        match self {
            Self::Minimal => &MINIMAL,
            Self::Glyph => &GLYPH,
        }
    }
}

#[cfg(not(test))]
static STYLE: AtomicU8 = AtomicU8::new(0);

// Tests run in parallel and several of them flip the style; a per-thread
// store keeps one test's Glyph frames out of another's Minimal ones.
#[cfg(test)]
thread_local! {
    static STYLE: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

pub fn set_ui_style(style: UiStyle) {
    #[cfg(not(test))]
    STYLE.store(style as u8, Ordering::Relaxed);
    #[cfg(test)]
    STYLE.with(|cell| cell.set(style as u8));
}

pub fn ui_style() -> UiStyle {
    #[cfg(not(test))]
    let raw = STYLE.load(Ordering::Relaxed);
    #[cfg(test)]
    let raw = STYLE.with(std::cell::Cell::get);
    match raw {
        1 => UiStyle::Glyph,
        _ => UiStyle::Minimal,
    }
}

/// The active style's table.
pub fn glyphs() -> &'static Glyphs {
    ui_style().glyphs()
}

pub struct Glyphs {
    /// Queued or not yet started.
    pub waiting: char,
    /// A tool row while its call runs; indexed by the spinner frame.
    pub running: &'static [char],
    /// A section label or the status spinner while the run is live.
    pub active: &'static [char],
    /// A tool row that finished; drawn in the success color.
    pub done: char,
    /// A tool row that failed; drawn in the error color.
    pub failed: char,
    /// A section label at rest, and the idle run state. Distinct from
    /// `done` so a finished section does not read as one more tool.
    pub section: char,
    /// The model is waiting on the person: approval, clarification.
    pub attention: char,
    /// Whether the status bar's run state carries a mark.
    pub status_marks: bool,
    /// User prompt, first-level heading, selected row.
    pub rail: &'static str,
    /// Appended to the last line of streaming text; empty for none.
    pub caret: &'static str,
    /// Context meter as (used, free); `None` shows the percentage only.
    pub meter: Option<(char, char)>,
    /// Markdown heading prefixes by level (h1, h2, h3).
    pub heading: [&'static str; 3],
    /// Selected picker row marker.
    pub cursor: &'static str,
    /// Prefix of the finished-turn footer; empty for none. Both styles
    /// leave it empty: a mark on a dim summary row only pulls the eye.
    pub footer: &'static str,
}

impl Glyphs {
    /// The live section mark for one spinner frame.
    pub fn active_frame(&self, frame: usize) -> char {
        self.active[frame % self.active.len()]
    }

    /// The running tool mark for one spinner frame.
    pub fn running_frame(&self, frame: usize) -> char {
        self.running[frame % self.running.len()]
    }

    /// The status bar's prefix for a run state: empty in a text-only style.
    pub fn status_prefix(&self, mark: char) -> String {
        if self.status_marks {
            format!("{mark} ")
        } else {
            String::new()
        }
    }

    /// A fixed-width meter of `cells` cells, or `None` for a text-only style.
    pub fn meter_bar(&self, cells: usize, ratio: f64) -> Option<String> {
        let (used, free) = self.meter?;
        let ratio = ratio.clamp(0.0, 1.0);
        // Anything used shows at least one cell: an empty bar says zero.
        let filled = ((ratio * cells as f64).round() as usize)
            .max(usize::from(ratio > 0.0))
            .min(cells);
        let mut bar = String::with_capacity(cells * used.len_utf8());
        bar.extend(std::iter::repeat_n(used, filled));
        bar.extend(std::iter::repeat_n(free, cells - filled));
        Some(bar)
    }
}

pub static MINIMAL: Glyphs = Glyphs {
    waiting: '□',
    running: &['□'],
    active: &['·', ' '],
    done: '✓',
    failed: '×',
    section: '•',
    attention: '?',
    status_marks: false,
    rail: "┃",
    caret: "",
    meter: None,
    heading: ["# ", "## ", "### "],
    cursor: "▸",
    footer: "",
};

pub static GLYPH: Glyphs = Glyphs {
    waiting: '□',
    // Tool rows keep the Minimal marks: a tick and a cross read at a
    // glance, and the squares stay for sections, the run state, and rails.
    running: &['□'],
    active: &['◧', '◨'],
    done: '✓',
    failed: '×',
    section: '□',
    attention: '◫',
    status_marks: true,
    rail: "┃",
    caret: "▏",
    meter: Some(('━', '─')),
    heading: ["┃ ", "│ ", ""],
    cursor: "┃",
    footer: "",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_style_fills_every_slot() {
        for style in UiStyle::ALL {
            let g = style.glyphs();
            assert!(
                !g.active.is_empty() && !g.running.is_empty(),
                "{}",
                style.label()
            );
            assert!(!g.rail.is_empty());
            assert!(!g.cursor.is_empty());
            assert!(g.heading.iter().take(2).all(|h| !h.is_empty()));
            assert_eq!(style, UiStyle::from_slug(style.slug()).unwrap());
        }
    }

    #[test]
    fn meter_rounds_to_the_cell_count() {
        assert_eq!(GLYPH.meter_bar(8, 0.30).as_deref(), Some("━━──────"));
        assert_eq!(GLYPH.meter_bar(8, 0.0).as_deref(), Some("────────"));
        assert_eq!(GLYPH.meter_bar(8, 0.02).as_deref(), Some("━───────"));
        assert_eq!(GLYPH.meter_bar(8, 1.5).as_deref(), Some("━━━━━━━━"));
        assert_eq!(MINIMAL.meter_bar(8, 0.5), None);
    }

    /// State marks live only in this table. A renderer that spells one
    /// inline has left the vocabulary, and the other style will not follow.
    #[test]
    fn renderers_do_not_spell_state_marks_inline() {
        let sources = [
            ("render/shell.rs", include_str!("../tui/render/shell.rs")),
            (
                "render/transcript.rs",
                include_str!("../tui/render/transcript.rs"),
            ),
            (
                "render/pickers.rs",
                include_str!("../tui/render/pickers.rs"),
            ),
            (
                "render/overlays.rs",
                include_str!("../tui/render/overlays.rs"),
            ),
            (
                "components/tool_row.rs",
                include_str!("../tui/components/tool_row.rs"),
            ),
            (
                "components/section.rs",
                include_str!("../tui/components/section.rs"),
            ),
            (
                "components/status_bar.rs",
                include_str!("../tui/components/status_bar.rs"),
            ),
            (
                "components/approval.rs",
                include_str!("../tui/components/approval.rs"),
            ),
            (
                "components/ask.rs",
                include_str!("../tui/components/ask.rs"),
            ),
            (
                "components/activity_rail.rs",
                include_str!("../tui/components/activity_rail.rs"),
            ),
            (
                "components/subagent_row.rs",
                include_str!("../tui/components/subagent_row.rs"),
            ),
            (
                "components/progress_list.rs",
                include_str!("../tui/components/progress_list.rs"),
            ),
        ];
        let marks = ['◧', '◨', '■', '◫', '▏', '━'];
        for (name, source) in sources {
            for mark in marks {
                assert!(
                    !source.contains(mark),
                    "{name} spells {mark:?} inline; add it to the Glyphs table"
                );
            }
        }
    }
}
