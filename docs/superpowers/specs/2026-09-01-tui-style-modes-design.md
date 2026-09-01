# TUI style modes: Minimal and Glyph

Date: 2026-09-01. Scope: `crates/cli` only. No other crate is touched.

## Goal

Make the default TUI cleaner (Minimal) and add an opt-in Glyph style with
a single coherent mark family, a context meter, and one-cell animations.
The style is a persisted preference toggled from `/settings`.

## Constraints

- Only files under `crates/cli/src/tui`, `crates/cli/src/view`, and the
  preference accessors in `crates/cli/src/config/storage.rs` change.
  Harness, tools, extensions, and model-provider crates are untouched.
- Renderers never spell a glyph inline. Every mark comes from the
  `Glyphs` table so both styles fill every slot (DRY, and the coherence
  guarantee).
- Animations change one cell per row on the existing 120 ms ticker. The
  no-clone transcript path and priority-dropping status bar stay as they
  are.
- Boy scout rule: code touched for this work is left tidier than found
  (dead helpers removed, duplicated formatting collapsed into the shared
  helpers introduced here), without unrelated refactors.

## The `UiStyle` preference

```rust
pub enum UiStyle { Minimal, Glyph }
```

Mirrors `TranscriptSpacing` (`tui/components/transcript.rs`):

- `ALL`, `label()` ("Minimal" / "Glyph"), `slug()` ("minimal" / "glyph"),
  `stored()` reading config key `style`, process-global atomic with
  `ui_style()` / `set_ui_style()`.
- `config/storage.rs`: `stored_style()` / `save_style(slug)` next to
  `stored_transcript_spacing`. The table itself lives in
  `view/glyphs.rs` beside the theme, so `view` stays free of `tui`.
- `/settings` gains a ninth row `style` after `spacing`
  (`render/pickers.rs::settings_lines`, `SETTINGS_ROWS = 9`). Enter opens
  `Overlay::Style { picker }`; `keys/overlays.rs` maps it to
  `After::CloseAndSetStyle(UiStyle)`; `keys/overlays/apply.rs` sets the
  atomic, saves, and posts a notice like the spacing path does.
- The picker rows show the label and a one-line description:
  `Minimal  plain marks, text-only status` /
  `Glyph    square state marks, rails, context meter`.
- No slash command. `/settings` is the only entry point.

## The `Glyphs` table

```rust
pub struct Glyphs {
    pub waiting: char,            // queued / pending
    pub running: &'static [char], // a tool row while it runs
    pub active: &'static [char],  // section label / spinner while live
    pub done: char,               // finished tool, success color
    pub failed: char,             // failed tool, error color
    pub section: char,            // section label at rest, idle state
    pub attention: char,          // approval, ask
    pub status_marks: bool,       // status bar run state carries a mark
    pub rail: &'static str,       // user prompt, selected ask row
    pub caret: &'static str,      // end of streaming text
    pub meter: Option<(char, char)>, // (used, free); None = percentage only
    pub heading: [&'static str; 3],  // h1, h2, h3 prefixes
    pub cursor: &'static str,     // selected picker row
    pub footer: &'static str,     // turn footer prefix
}
pub fn glyphs() -> &'static Glyphs   // by ui_style()
```

| Slot        | Minimal (default) | Glyph      |
|-------------|-------------------|------------|
| waiting     | `□`               | `□`        |
| running     | `□`               | `□`        |
| active      | `·` / ` ` (blink) | `◧` / `◨`  |
| done        | `✓` (success)     | `✓` (success) |
| failed      | `×` (error)       | `×` (error)   |
| section     | `•`               | `□`        |
| attention   | `?`               | `◫`        |
| rail        | `┃`               | `┃`        |
| caret       | none              | `▏`        |
| meter       | none              | `━` / `─` (6 cells) |
| cursor      | `▸`               | `┃`        |
| footer      | none              | none       |

Glyph family rules: state marks are same-size geometric squares
(U+25A1, 25A0, 25E7, 25E8, 25EB) for sections, the run state and the
footer; tool rows keep the Minimal tick, cross and hollow square so a
result reads at a glance; structure is box-drawing strokes;
measure is heavy/light horizontal strokes. The logo `▀▄` remains the only
block element. Headings in Glyph: h1 `┃ Title`, h2 `│ Title`, h3 bold
only. Headings in Minimal: `# Title`, `## Title`, `### Title`.

Where a mark appears:

- Tool rows and the Work/Thinking section label (the label carries the
  roll-up state: `active` while any tool runs, `done` after).
- Queue block header (`waiting`).
- Live spinner row and status-bar run state (`active` running, `done`
  idle, `attention` awaiting approval or answer).
- Approval and ask headers (`attention`); ask question and option
  gutter (`cursor` on the focused row).
- Picker selected row (`cursor`: `▸` in Minimal, `┃` in Glyph).
- Streaming assistant text (`caret` appended to the last live line).
- Status-bar context segment: `ctx ━━──── 30%` in Glyph. Segments can
  carry a compact form; a tight row takes compact forms in bar order
  (the meter, then the hint's last key) before it drops any segment.
  Minimal shows `ctx 30%` only.

## Content fixes (both styles)

1. Tool rows (`format.rs`, `components/tool_row.rs`): per-tool call and
   result summaries: `grep "priority" in crates/cli/src/tui · 7 matches`,
   `read_file path · 5.1 kB`, `edit_file path · 1 replacement`,
   `shell $ cmd · exit 0`. Generic fallback: first string-valued input
   field for the call, `ok` / first string field for the result; never
   serialized JSON. Timing: one duration, execution time when known, else
   total; hidden under 1 ms; the word `preflight` is gone.
2. Markdown (`view/markdown.rs`): headings get the style's prefix; inline
   code keeps its color and, under the `Mono` theme, its backticks.
3. Turn footer (`events/ui_message.rs`, `render/transcript.rs`):
   `done · 0.2s · 2 tools · ↑12.0k ↓611`, rendered one row under the
   answer and scrolling with it; no mark in either style.
   The status bar keeps its short run state; mirroring the footer there
   crowded the never-dropped segment. `shell · 0.3s` follows the same
   shape.
4. Token vocabulary: one `format_tokens` helper (`12.0k`, `611`) used by
   the spinner row, footer, status bar, and usage panel.
5. Palette and help rows (`render/pickers.rs`): category as a dim tag
   right after the command name; the description takes the remaining
   width.
6. Split view (`render/shell.rs`): dim header on the transcript pane
   (`transcript · turn N`); status bar keeps the model name and drops
   `wheel scroll` first.
7. Welcome card (`components/welcome.rs`): stays centred; `/help`,
   `/models` and `/mode` hints become rows aligned with the `model` /
   `workspace` labels.

Not changed: approval legend layout, ask form layout, queue block,
tree connectors, markdown bullets, status separators.

## Testing

Frame tests in `tui/tests/render_frames.rs`, run for both styles by
setting the atomic in the test:

- tool row summaries and hidden sub-ms timings;
- footer directly under the answer, nothing left in the live region;
- context meter present at 140 columns, percentage at 64;
- caret on the last streaming line in Glyph, absent in Minimal;
- welcome card centred with aligned hint rows;
- palette rows: tag placement and no truncation with free width.

Settings tests (`tui/tests/core_6.rs` pattern): row nine opens the style
picker, enter persists `style`, `ui_style()` flips, `UiStyle::stored()`
reads it back.

Glyph table test: every `Glyphs` constant fills every slot and no
renderer source file contains a literal from the state-mark set outside
`glyphs.rs` (a grep-style unit test over `include_str!`).

## Order of work

1. `UiStyle` + `Glyphs` + `/settings` row + tests.
2. Content fixes 1–4 through the table.
3. Glyph-only rendering: caret, meter, rails, section roll-up state.
4. Content fixes 5–7.
