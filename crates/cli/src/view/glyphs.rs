//! The mark vocabulary, chosen once per process by the `style` preference.
//!
//! Renderers never spell a state mark inline: they read the table so both
//! styles fill every slot and the marks stay one family. The Glyph style
//! uses compact geometric state marks, box-drawing strokes for structure,
//! and heavy/light rules for measure; the logo's `▀▄` stays the only block
//! element on screen.

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

    /// The persisted choice, Glyph when none is saved.
    pub fn stored() -> Self {
        crate::config::stored_style()
            .as_deref()
            .and_then(Self::from_slug)
            .unwrap_or(Self::Glyph)
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

/// The shape of the state marks, a preference beside the style: squares
/// (`□ ■`) or circles (`○ ●`). Either way hollow is work and solid is
/// done; only the outline changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkShape {
    Square,
    Circle,
}

impl MarkShape {
    pub const ALL: [Self; 2] = [Self::Square, Self::Circle];

    pub fn label(self) -> &'static str {
        match self {
            Self::Square => "Square",
            Self::Circle => "Circle",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Square => "square",
            Self::Circle => "circle",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Square => "□ running, ■ done",
            Self::Circle => "○ running, ● done",
        }
    }

    pub fn from_slug(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|shape| shape.slug() == value)
    }

    /// The persisted choice, Square when none is saved.
    pub fn stored() -> Self {
        crate::config::stored_marks()
            .as_deref()
            .and_then(Self::from_slug)
            .unwrap_or(Self::Square)
    }
}

#[cfg(not(test))]
static SHAPE: AtomicU8 = AtomicU8::new(0);

#[cfg(test)]
thread_local! {
    static SHAPE: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

pub fn set_mark_shape(shape: MarkShape) {
    #[cfg(not(test))]
    SHAPE.store(shape as u8, Ordering::Relaxed);
    #[cfg(test)]
    SHAPE.with(|cell| cell.set(shape as u8));
}

pub fn mark_shape() -> MarkShape {
    #[cfg(not(test))]
    let raw = SHAPE.load(Ordering::Relaxed);
    #[cfg(test)]
    let raw = SHAPE.with(std::cell::Cell::get);
    match raw {
        1 => MarkShape::Circle,
        _ => MarkShape::Square,
    }
}

/// How a rail's last branch turns: a square elbow (`└─`) or a rounded
/// curve (`╰─`). Mid-rail rows keep `├─` either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchShape {
    Elbow,
    Curve,
}

impl BranchShape {
    pub const ALL: [Self; 2] = [Self::Elbow, Self::Curve];

    pub fn label(self) -> &'static str {
        match self {
            Self::Elbow => "Elbow",
            Self::Curve => "Curve",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Elbow => "elbow",
            Self::Curve => "curve",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Elbow => "├─ row, └─ last row",
            Self::Curve => "├─ row, ╰─ last row",
        }
    }

    /// The corner the last row's branch starts with.
    pub fn corner(self) -> &'static str {
        match self {
            Self::Elbow => "└",
            Self::Curve => "╰",
        }
    }

    pub fn from_slug(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|shape| shape.slug() == value)
    }

    /// The persisted choice, Elbow when none is saved.
    pub fn stored() -> Self {
        crate::config::stored_branches()
            .as_deref()
            .and_then(Self::from_slug)
            .unwrap_or(Self::Elbow)
    }
}

#[cfg(not(test))]
static BRANCH: AtomicU8 = AtomicU8::new(0);

#[cfg(test)]
thread_local! {
    static BRANCH: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

pub fn set_branch_shape(shape: BranchShape) {
    #[cfg(not(test))]
    BRANCH.store(shape as u8, Ordering::Relaxed);
    #[cfg(test)]
    BRANCH.with(|cell| cell.set(shape as u8));
}

pub fn branch_shape() -> BranchShape {
    #[cfg(not(test))]
    let raw = BRANCH.load(Ordering::Relaxed);
    #[cfg(test)]
    let raw = BRANCH.with(std::cell::Cell::get);
    match raw {
        1 => BranchShape::Curve,
        _ => BranchShape::Elbow,
    }
}

/// The active style's table, in the active mark shape.
pub fn glyphs() -> &'static Glyphs {
    match (ui_style(), mark_shape()) {
        (style, MarkShape::Square) => style.glyphs(),
        (UiStyle::Minimal, MarkShape::Circle) => &MINIMAL_ROUND,
        (UiStyle::Glyph, MarkShape::Circle) => &GLYPH_ROUND,
    }
}

pub struct Glyphs {
    /// Queued or not yet started.
    pub waiting: char,
    /// A tool row while its call runs; indexed by the spinner frame.
    pub running: &'static [char],
    /// A section label or the status spinner while the run is live.
    pub active: &'static [char],
    /// A tool row that finished; drawn in the success color. In the Glyph
    /// style it is the solid square: hollow is work, solid is done.
    pub done: char,
    /// A tool row that failed; drawn in the error color.
    pub failed: char,
    /// A section label at rest, and the idle run state. The Glyph style
    /// shares `done`'s solid square (a section at rest is finished) and
    /// tells them apart by colour: dim here, success on a tool row.
    pub section: char,
    /// A live background process count in the status bar.
    pub process: char,
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
    /// The workspace root on the status line; empty for none.
    pub workspace: &'static str,
    /// The git branch on the status line; empty for none.
    pub branch: &'static str,
    /// Selected picker row marker.
    pub cursor: &'static str,
    /// Hangs a failed row's error detail off its branch; a space where the
    /// style draws no structure.
    pub hook: &'static str,
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

    /// The status bar's trailing segment: where the session is running.
    ///
    /// A marked style nests the branch inside the workspace — `⌂ orca
    /// (⑂ main)` — so the two read as one place rather than as two more
    /// `·`-joined segments. A text-only style keeps the flat join, which
    /// is what it has always shown.
    pub fn workspace_label(&self, name: &str, branch: Option<&str>) -> String {
        match (self.workspace.is_empty(), branch) {
            (true, Some(branch)) => format!("{name} · {branch}"),
            (true, None) => name.to_string(),
            (false, Some(branch)) => {
                format!("{} {name} ({} {branch})", self.workspace, self.branch)
            }
            (false, None) => format!("{} {name}", self.workspace),
        }
    }

    /// The trailing segment's compact form. The folder name is the most
    /// recoverable thing on the row — the branch is what the person
    /// actually needs — so it is the first thing the bar gives up, ahead
    /// of the context meter.
    pub fn workspace_compact(&self, name: &str, branch: Option<&str>) -> String {
        match branch {
            Some(branch) if self.branch.is_empty() => branch.to_string(),
            Some(branch) => format!("{} {branch}", self.branch),
            None => self.workspace_label(name, None),
        }
    }

    /// A count segment: `✓ 2/5` where marks are on, `todo 2/5` where the
    /// style is text-only. The mark replaces the word rather than joining
    /// it, so a marked bar is never the wider of the two.
    pub fn counted(&self, mark: char, word: &str, body: &str) -> String {
        if self.status_marks {
            format!("{mark} {body}")
        } else {
            format!("{word} {body}")
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

pub static MINIMAL: Glyphs = MINIMAL_TABLE;

/// Minimal with circles: only the hollow work mark changes; its done
/// mark is a check and its section mark already a dot.
pub static MINIMAL_ROUND: Glyphs = Glyphs {
    waiting: '○',
    running: &['○'],
    ..MINIMAL_TABLE
};

const MINIMAL_TABLE: Glyphs = Glyphs {
    waiting: '□',
    running: &['□'],
    active: &['·', ' '],
    done: '✓',
    failed: '×',
    section: '•',
    process: '•',
    attention: '?',
    status_marks: false,
    rail: "┃",
    caret: "",
    meter: None,
    heading: ["# ", "## ", "### "],
    workspace: "",
    branch: "",
    cursor: "▸",
    hook: " ",
    footer: "",
};

pub static GLYPH: Glyphs = GLYPH_TABLE;

/// Glyph with circles: the same hollow-work, solid-done rule in `○ ●`.
pub static GLYPH_ROUND: Glyphs = Glyphs {
    waiting: '○',
    running: &['○'],
    active: &['○', '●'],
    done: '●',
    section: '●',
    ..GLYPH_TABLE
};

const GLYPH_TABLE: Glyphs = Glyphs {
    waiting: '□',
    running: &['□'],
    active: &['□', '■'],
    done: '■',
    failed: '×',
    section: '■',
    process: '⚙',
    attention: '!',
    status_marks: true,
    rail: "┃",
    caret: "▏",
    meter: Some(('━', '─')),
    heading: ["┃ ", "│ ", ""],
    workspace: "\u{2302}",
    branch: "\u{2442}",
    cursor: "›",
    hook: "╰",
    footer: "",
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Circles keep the square tables' rule: hollow is work, solid is
    /// done, one cell each, and everything but the state marks unchanged.
    #[test]
    fn circles_swap_only_the_state_marks() {
        for (square, round) in [(&GLYPH, &GLYPH_ROUND), (&MINIMAL, &MINIMAL_ROUND)] {
            assert_eq!(round.waiting, '○');
            assert!(round.running.iter().all(|mark| *mark == '○'));
            assert_eq!(round.failed, square.failed);
            assert_eq!(round.cursor, square.cursor);
            assert_eq!(round.rail, square.rail);
            for mark in [round.waiting, round.done, round.section] {
                assert_eq!(crate::view::cell_width(&mark.to_string()), 1);
            }
        }
        assert_eq!((GLYPH_ROUND.done, GLYPH_ROUND.section), ('●', '●'));
        assert_eq!(GLYPH_ROUND.active, &['○', '●']);
        assert_eq!(MINIMAL_ROUND.done, MINIMAL.done);
        set_ui_style(UiStyle::Glyph);
        set_mark_shape(MarkShape::Circle);
        assert_eq!(glyphs().done, '●');
        set_mark_shape(MarkShape::Square);
        assert_eq!(glyphs().done, '■');
        set_ui_style(UiStyle::Minimal);
    }

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

    /// The status bar's place segment: one reading in both styles, with
    /// the marked one nesting the branch instead of adding a segment.
    #[test]
    fn the_workspace_names_a_place_in_both_styles() {
        assert_eq!(MINIMAL.workspace_label("orca", Some("main")), "orca · main");
        assert_eq!(MINIMAL.workspace_label("orca", None), "orca");
        assert_eq!(
            GLYPH.workspace_label("orca", Some("main")),
            "⌂ orca (⑂ main)"
        );
        assert_eq!(GLYPH.workspace_label("orca", None), "⌂ orca");
    }

    /// The folder name is the first thing the row gives up, so the
    /// compact form keeps the branch — the half that changes.
    #[test]
    fn the_workspace_compacts_to_its_branch() {
        assert_eq!(MINIMAL.workspace_compact("orca", Some("main")), "main");
        assert_eq!(MINIMAL.workspace_compact("orca", None), "orca");
        assert_eq!(GLYPH.workspace_compact("orca", Some("main")), "⑂ main");
        assert_eq!(GLYPH.workspace_compact("orca", None), "⌂ orca");
        for style in UiStyle::ALL {
            let g = style.glyphs();
            assert!(
                crate::view::cell_width(&g.workspace_compact("orca", Some("main")))
                    <= crate::view::cell_width(&g.workspace_label("orca", Some("main"))),
                "{} compact is not shorter",
                style.label()
            );
        }
    }

    /// A mark replaces the word rather than joining it: a marked count is
    /// never wider than the text-only one it stands in for.
    #[test]
    fn a_counted_mark_never_costs_more_than_its_word() {
        assert_eq!(MINIMAL.counted(MINIMAL.done, "todo", "2/5"), "todo 2/5");
        assert_eq!(GLYPH.counted(GLYPH.done, "todo", "2/5"), "■ 2/5");
        assert_eq!(MINIMAL.counted(MINIMAL.waiting, "q", "3"), "q 3");
        assert_eq!(GLYPH.counted(GLYPH.waiting, "q", "3"), "□ 3");
        assert_eq!(MINIMAL.counted(MINIMAL.process, "procs", "2"), "procs 2");
        assert_eq!(GLYPH.counted(GLYPH.process, "procs", "2"), "⚙ 2");
        for (word, body) in [("todo", "2/5"), ("q", "3")] {
            assert!(
                crate::view::cell_width(&GLYPH.counted(GLYPH.done, word, body))
                    <= crate::view::cell_width(&MINIMAL.counted(MINIMAL.done, word, body))
            );
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
        ];
        // `tool_row.rs` receives its glyph from callers; it never chooses a
        // state mark. Its test fixture also uses `○` as a connector.
        // `!` is ordinary Rust syntax and `□` is the shared Minimal waiting
        // mark, so the table-level tests cover their assigned values.
        let marks = ['⬚', '■', '›', '▏', '━', '⌂', '⑂'];
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
