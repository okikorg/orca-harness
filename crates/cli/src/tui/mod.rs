//! The interactive terminal. Fullscreen alternate-screen app: the
//! transcript fills the window from the top, a live region (streaming
//! tail, approval prompts, the slash palette) sits above the composer,
//! and the composer plus status line are pinned to the bottom. The wheel,
//! shift+↑/↓, and PgUp/PgDn all scroll the in-app transcript buffer.
//!
//! Mouse capture stays on: a terminal forwards the wheel to a fullscreen
//! app only under capture. Selection is not lost to it — terminals reserve
//! a modifier drag for their own selection while an app holds the mouse
//! (option-drag on macOS terminals, shift-drag elsewhere) — and `/copy`
//! (ctrl+y) writes to the clipboard through the terminal, reaching content
//! that has scrolled past, which no drag can.
mod format;
mod input;
mod inspector;
mod render;

pub(crate) use self::render::strip_location_mentions;

use std::collections::VecDeque;
use std::io;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CtEvent, EventStream as CtEventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::Terminal;
use tokio::sync::mpsc;

use orca_harness_core::CancellationToken;
use orca_harness_extensions::HarnessEvent;
use orca_harness_model_openrouter::ModelInfo;

use crate::clipboard;
use crate::commands::{filter_commands, CommandSpec};
use crate::components::picker::{ListPicker, PickerAction, PickerEvent};
use crate::msg::{ApprovalRequest, ApprovalResponse, Provider, UiMsg, WorkerCmd};
use crate::view::{self, theme};

#[cfg(test)]
use self::format::redact_command;
use self::format::{byte_index, elapsed_label, size};
use self::input::{
    expand_pastes, insert_paste, marker_ending_at, marker_starting_at, remove_marker,
};
use self::inspector::inspector_text_content;
#[cfg(test)]
use self::inspector::{
    inspector_code_facts, inspector_output_preview, shallow_json_preview, tool_inspector_lines,
};
#[cfg(test)]
use self::render::{
    activity_lines, context_segment, live_lines, mode_segment, palette_lines, projected_transcript,
    stabilize_transcript_scroll, stats_segments, todo_segment,
};
use self::render::{
    activity_lines_selected, collapsed_activity_lines, draw, matching_indices, mention_starts_at,
    remove_location_mention_before_cursor, replay_transcript, reset_conversation_ui,
    transcript_content_width, workspace_locations,
};
#[cfg(test)]
use ratatui::layout::{Constraint, Layout};

const SPINNER: &[char] = &['·', ' '];
const EXPAND_MAX_LINES: usize = 200;
const TRANSCRIPT_CAP: usize = 5000;
const INSPECTOR_PREVIEW_LINES: usize = 240;
const INSPECTOR_PREVIEW_CHARS: usize = 32 * 1024;
const INSPECTOR_OUTPUT_HEAD: usize = 16;
const INSPECTOR_OUTPUT_TAIL: usize = 6;
const SCROLL_PAGE: usize = 10;
/// How long the selection hint stays up after a scroll.
const SCROLL_HINT: Duration = Duration::from_secs(6);
const PALETTE_ROWS: usize = 8;
const PICKER_ROWS: usize = 10;
/// How many sessions the /sessions picker displays at once (newest
/// first); the cursor pages through the rest like the /models picker.
const SESSIONS_WINDOW: usize = 5;
const LIVE_TOOL_ROWS: usize = 8;
const QUEUE_PREVIEW_ROWS: usize = 3;

/// The active theme is process-global, so the test that switches it and
/// the tests that assert theme-derived colors must not interleave. Both
/// take this lock; without it they race whenever the harness happens to
/// schedule them together.
#[cfg(test)]
static THEME_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
pub struct TuiConfig {
    pub model_name: String,
    pub workspace_name: String,
    /// Canonicalized workspace root: keys this workspace's saved tool
    /// approvals in the config file.
    pub workspace_root: String,
    /// The active endpoint provider; provider switches update it.
    pub provider: Provider,
    /// Shared handle behind the `subagent` tool's nesting cap;
    /// `/subagents` adjusts it live.
    pub subagent_depth: orca_harness_tools::SubagentDepth,
    /// Live background-work counters rendered in the status line.
    pub stats: orca_harness_tools::BackgroundStats,
    /// The recording session's id, if recording; `/sessions` marks it.
    pub session_id: Option<String>,
    /// Shared handle onto the connected MCP servers; `/mcp` reads each
    /// one's tool count and connection error from it.
    pub mcp: crate::mcp::McpServers,
    /// Shared handle onto the scanned skills; `/skills` renders the
    /// catalog, the shadowed copies, and the parse failures from it.
    pub skills: crate::skills::Skills,
    /// Shared handle onto the session mode; `/mode` flips it and the
    /// status line shows it. Read by the plan gate on every tool call,
    /// so a flip applies to the call in flight, not to the next run.
    pub mode: crate::mode::ModeHandle,
    /// Shared handle onto the agent's task list, written by `todo_write`
    /// and rendered by `/todo`.
    pub todos: orca_harness_tools::TodoList,
    /// The plan artifact the current planning episode may write. Leaving
    /// plan mode ends the episode and reports where the plan went.
    pub plan: crate::plan::PlanArea,
}

/// The interactive model selector: the fetched catalog, a live-typed
/// filter, and the selected row (an index into the filtered view).
struct ModelPicker {
    models: Vec<ModelInfo>,
    filter: String,
    index: usize,
}

/// Workspace-relative file and directory inserted into the composer by
/// the `@` mention picker.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LocationEntry {
    path: String,
    directory: bool,
}

struct LocationPicker {
    entries: Vec<LocationEntry>,
    query: String,
    /// Character offset of the `@` that opened this picker.
    token_start: usize,
    picker: ListPicker,
}

impl LocationPicker {
    fn filtered(&self) -> Vec<&LocationEntry> {
        let needle = self.query.to_lowercase();
        self.entries
            .iter()
            .filter(|entry| needle.is_empty() || entry.path.to_lowercase().contains(&needle))
            .take(PICKER_ROWS)
            .collect()
    }

    fn selected(&self) -> Option<LocationEntry> {
        self.filtered().get(self.picker.index()).cloned().cloned()
    }

    fn sync_len(&mut self) {
        let len = self.filtered().len();
        self.picker.set_len(len);
    }
}

impl ModelPicker {
    fn filtered(&self) -> Vec<&ModelInfo> {
        let needle = self.filter.to_lowercase();
        self.models
            .iter()
            .filter(|m| needle.is_empty() || m.id.to_lowercase().contains(&needle))
            .collect()
    }

    /// The id under the cursor, if any model matches the filter.
    /// The selected model's id and catalog-reported context window.
    fn selected_info(&self) -> Option<(String, Option<u64>)> {
        let filtered = self.filtered();
        filtered
            .get(self.index.min(filtered.len().saturating_sub(1)))
            .map(|m| (m.id.clone(), m.context_length))
    }
}

/// A modal selector rendered in the live region. Approval prompts win
/// over overlays; overlays win over the slash palette.
enum Overlay {
    /// Model selector over the fetched catalog.
    Models(ModelPicker),
    /// Workspace file/folder selector opened by typing `@` in the composer.
    Locations(LocationPicker),
    /// Provider selector (openrouter, openai, local).
    Providers { picker: ListPicker },
    /// Theme selector over `view::ThemeName::ALL`.
    Themes { picker: ListPicker },
    /// Transcript layout selector (classic or split inspector).
    Views { picker: ListPicker },
    /// Read-only session usage panel; any dismissal key closes it.
    Usage,
    /// Masked API-key entry for a provider whose key is not in the env.
    ApiKey { provider: Provider, input: String },
    /// Settings menu: shows the persisted preferences and jumps into
    /// the provider, model, theme, api-key, and approval pickers.
    Settings { picker: ListPicker },
    /// This workspace's saved always-allowed tools; enter revokes one.
    Approvals {
        tools: Vec<String>,
        picker: ListPicker,
    },
    /// The harness extension catalog; enter toggles the selected one.
    Extensions { picker: ListPicker },
    /// The configured MCP servers; space (or enter) toggles the
    /// selected one on or off. Rows come from the config, so a toggle
    /// redraws immediately while the reconnect runs behind it.
    Mcp {
        servers: Vec<crate::config::McpServer>,
        filter: String,
        picker: ListPicker,
    },
    /// The skills found on disk; space (or enter) turns the selected one
    /// on or off. Rows are a snapshot of the last scan, so a toggle
    /// redraws immediately while the rescan runs behind it.
    Skills {
        entries: Vec<crate::skills::SkillEntry>,
        filter: String,
        picker: ListPicker,
    },
    /// Recorded sessions for this workspace; enter resumes the selection.
    Sessions {
        sessions: Vec<orca_harness_extensions::SessionFile>,
        picker: ListPicker,
    },
}

/// Rows in the settings overlay: provider, model, theme, transcript view,
/// api key, approvals.
const SETTINGS_ROWS: usize = 6;

/// Row actions in the /sessions picker (space arms them).
const SESSION_ACTIONS: &[PickerAction] = &[PickerAction {
    key: 'd',
    label: "delete",
}];

/// Row actions in the /skills picker. Enter still toggles — the common
/// case stays one key — and space reveals the rest, so deleting a skill
/// is never one stray keystroke away.
const SKILL_ACTIONS: &[PickerAction] = &[
    PickerAction {
        key: 't',
        label: "toggle",
    },
    PickerAction {
        key: 'd',
        label: "delete",
    },
];

/// A finished tool call kept around so the user can expand its full
/// output later with `/expand n`.
struct ToolRecord {
    call_line: String,
    tool_name: String,
    output: serde_json::Value,
    /// Folded inner tool log of a finished subagent call, empty otherwise.
    inner: Vec<String>,
}

/// A completed turn's compacted work rail. The transcript keeps only the
/// summary until the user asks to inspect the tree with ctrl+o.
struct CompletedWork {
    turn: usize,
    /// Consecutive collapsed rows occupying this phase's place in the
    /// transcript. Kept verbatim so ctrl+o can replace the whole block.
    summaries: Vec<String>,
    lines: Vec<Line<'static>>,
    expanded: bool,
}

struct ThinkingRecord {
    elapsed: Duration,
}

/// Vertical rhythm between transcript components. Model-provided leading or
/// trailing whitespace never participates in layout; the transcript owns it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockSpacing {
    Tight,
    Section,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Classic,
    Split,
}

impl ViewMode {
    const ALL: [Self; 2] = [Self::Classic, Self::Split];

    fn stored() -> Self {
        match crate::config::stored_view().as_deref() {
            Some("split") => Self::Split,
            _ => Self::Classic,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::Split => "Split",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Split => "split",
        }
    }
}

#[derive(Clone)]
struct ToolActivity {
    /// The model-assigned tool-call id (anchors nested subagent spawns).
    call_id: String,
    call_line: String,
    tool_name: String,
    input: serde_json::Value,
    started: Instant,
    elapsed: Option<Duration>,
    output: Option<serde_json::Value>,
    is_error: bool,
    approval: Option<String>,
}

struct InspectorBodyCache {
    call_id: String,
    complete: bool,
    is_error: bool,
    width: usize,
    lines: Vec<Line<'static>>,
}

/// One spawned inner agent's tool activity while it runs.
struct SpawnActivity {
    /// Tool-call id of the subagent call that spawned it.
    call_id: String,
    parent_id: Option<u64>,
    depth: u32,
    tools: Vec<ToolActivity>,
    /// Inner call id -> index into `tools`.
    pending: std::collections::HashMap<String, usize>,
}

enum RunState {
    Idle,
    Running {
        started: Instant,
        cancel: CancellationToken,
    },
}

struct App {
    cfg: TuiConfig,
    /// Lines waiting to move into the transcript on the next tick.
    pending_history: Vec<Line<'static>>,
    /// The full session transcript (capped at [`TRANSCRIPT_CAP`]).
    transcript: Vec<Line<'static>>,
    /// Lines scrolled up from the bottom of the transcript.
    scroll: usize,
    /// When the reader last scrolled. Drives the transient selection hint.
    scroll_hint_at: Option<Instant>,
    /// Previous wrapped overflow height. Used to keep the same top row
    /// anchored while live content or the composer region changes size.
    transcript_max_scroll: usize,
    reasoning: String,
    /// When the current reasoning phase started streaming.
    reasoning_started: Option<Instant>,
    thinking_log: Vec<ThinkingRecord>,
    text: String,
    pending_assistant: Option<String>,
    assistant_started: bool,
    run: RunState,
    approval: Option<ApprovalRequest>,
    composer: String,
    cursor: usize,
    /// Prompts waiting for the active turn to finish, oldest first.
    /// Held in marker form; expanded against [`App::pastes`] on send.
    prompt_queue: VecDeque<String>,
    /// Bulk pastes held aside, indexed by the number in their composer
    /// marker (`pastes[0]` is `#1`). Grows for the session: a queued or
    /// recalled prompt must still expand after later pastes arrive.
    pastes: Vec<String>,
    prompt_history: Vec<String>,
    history_pos: Option<usize>,
    tokens_in: u64,
    tokens_out: u64,
    cache_read_total: u64,
    cache_write_total: u64,
    /// Model steps that reported usage this session.
    usage_steps: u64,
    /// Approximate size of the model's current context, pi-style: the
    /// last step's provider-reported total, plus bytes/4 estimates for
    /// content appended since (tool results, the next prompt), plus
    /// /compact's estimate. Distinct from the cumulative session totals.
    context_tokens: u64,
    /// The active model's context window, when the catalog knows it.
    context_window: Option<u64>,
    spinner_frame: usize,
    quit: bool,
    tool_log: Vec<ToolRecord>,
    /// Completed per-turn work trees, newest last.
    work_log: Vec<CompletedWork>,
    /// Number of user turns rendered in this session.
    turn_count: usize,
    /// Top-level tool calls made during the current turn.
    turn_tool_calls: usize,
    /// Duration/tool-call summary of the last completed turn, shown in
    /// the rail above the composer until the next run starts.
    last_turn_summary: Option<String>,
    /// Call lines for in-flight tool calls, keyed by call id.
    pending_calls: std::collections::HashMap<String, usize>,
    activity_tools: Vec<ToolActivity>,
    view_mode: ViewMode,
    split_tool: Option<usize>,
    /// Last inspected call retained across model phases so Split never
    /// collapses or flashes while the next call is being prepared.
    split_snapshot: Option<ToolActivity>,
    split_inspector_cache: Option<InspectorBodyCache>,
    split_focused: bool,
    split_scroll: u16,
    /// Live inner activity of running subagents, keyed by spawn id.
    subagent_activity: std::collections::HashMap<u64, SpawnActivity>,
    /// Selected row in the slash-command palette.
    palette_index: usize,
    /// Open modal selector, if any.
    overlay: Option<Overlay>,
    /// Filter to seed the model picker with once the catalog reply arrives.
    picker_pending: Option<String>,
    /// Text waiting to be handed to the terminal's clipboard: decided
    /// here, written by the run loop between frames.
    clipboard_pending: Option<String>,
    /// The last complete answer the model produced, kept verbatim so
    /// `/copy` yields markdown source rather than the wrapped, styled,
    /// syntax-highlighted lines the transcript holds.
    last_answer: Option<String>,
}

impl App {
    fn new(cfg: TuiConfig) -> Self {
        Self {
            cfg,
            pending_history: Vec::new(),
            transcript: Vec::new(),
            scroll: 0,
            scroll_hint_at: None,
            transcript_max_scroll: 0,
            reasoning: String::new(),
            reasoning_started: None,
            thinking_log: Vec::new(),
            text: String::new(),
            pending_assistant: None,
            assistant_started: false,
            run: RunState::Idle,
            approval: None,
            composer: String::new(),
            cursor: 0,
            prompt_queue: VecDeque::new(),
            pastes: Vec::new(),
            prompt_history: Vec::new(),
            history_pos: None,
            tokens_in: 0,
            tokens_out: 0,
            cache_read_total: 0,
            cache_write_total: 0,
            usage_steps: 0,
            context_tokens: 0,
            context_window: None,
            spinner_frame: 0,
            quit: false,
            tool_log: Vec::new(),
            work_log: Vec::new(),
            turn_count: 0,
            turn_tool_calls: 0,
            last_turn_summary: None,
            pending_calls: std::collections::HashMap::new(),
            activity_tools: Vec::new(),
            view_mode: ViewMode::stored(),
            split_tool: None,
            split_snapshot: None,
            split_inspector_cache: None,
            split_focused: false,
            split_scroll: 0,
            subagent_activity: std::collections::HashMap::new(),
            palette_index: 0,
            overlay: None,
            picker_pending: None,
            clipboard_pending: None,
            last_answer: None,
        }
    }

    fn running(&self) -> bool {
        matches!(self.run, RunState::Running { .. })
    }

    /// The palette is open whenever the composer starts with `/` and no
    /// approval prompt is pending. Returns the text after the slash.
    fn palette_query(&self) -> Option<&str> {
        if self.approval.is_some() {
            return None;
        }
        self.composer.strip_prefix('/')
    }

    fn push_line(&mut self, line: Line<'static>) {
        self.pending_history.push(line);
    }

    fn push_wrapped(&mut self, text: &str, indent: &str, style: Style, width: usize) {
        push_wrapped_lines(&mut self.pending_history, text, indent, style, width);
    }

    fn push_record(&mut self, record: ToolRecord) {
        self.tool_log.push(record);
        if self.tool_log.len() > 100 {
            self.tool_log.remove(0);
        }
    }

    /// Finish one reasoning phase. The activity rail owns its compact
    /// rendering; the full text remains available through expansion.
    fn flush_reasoning(&mut self) {
        let reasoning = std::mem::take(&mut self.reasoning);
        let started = self.reasoning_started.take();
        if reasoning.trim().is_empty() {
            return;
        }
        let elapsed = started.map(|s| s.elapsed()).unwrap_or_default();
        let label = format!("thinking · {}", elapsed_label(elapsed));
        self.thinking_log.push(ThinkingRecord { elapsed });
        self.push_record(ToolRecord {
            call_line: label,
            tool_name: "thinking".into(),
            output: serde_json::Value::String(reasoning),
            inner: Vec::new(),
        });
    }

    fn reset_activity(&mut self) {
        self.turn_tool_calls = 0;
        self.last_turn_summary = None;
        self.reasoning.clear();
        self.reasoning_started = None;
        self.thinking_log.clear();
        self.activity_tools.clear();
        self.split_tool = None;
        self.split_focused = false;
        self.split_scroll = 0;
        self.subagent_activity.clear();
        self.pending_calls.clear();
        self.pending_assistant = None;
        self.assistant_started = false;
    }

    /// Render one transcript component with normalized outer edges and a
    /// single source of truth for vertical rhythm.
    fn push_transcript_block(&mut self, lines: Vec<Line<'static>>, spacing: BlockSpacing) {
        let spacing = if self.assistant_started {
            spacing
        } else {
            BlockSpacing::Section
        };
        let transcript_tail = self.transcript.last().map(line_is_blank);
        let appended =
            append_render_block(&mut self.pending_history, lines, spacing, transcript_tail);
        self.assistant_started |= appended;
    }

    fn push_markdown_block(&mut self, text: &str, width: usize, spacing: BlockSpacing) {
        self.push_transcript_block(view::markdown_lines(text, width, "  "), spacing);
    }

    fn commit_activity(&mut self, width: usize) {
        self.flush_reasoning();
        let selected = if !self.activity_tools.is_empty() {
            let selected = self
                .split_tool
                .unwrap_or_else(|| self.activity_tools.len().saturating_sub(1))
                .min(self.activity_tools.len().saturating_sub(1));
            self.split_snapshot = self.activity_tools.get(selected).cloned();
            (self.view_mode == ViewMode::Split).then_some(selected)
        } else {
            None
        };
        let lines = activity_lines_selected(self, width, false, selected);
        if !lines.is_empty() {
            // Work is part of the surrounding event stream, not a new
            // prose paragraph. Keep it adjacent to the narration and show
            // its useful rows immediately.
            self.push_transcript_block(lines.clone(), BlockSpacing::Tight);
            let collapsed = collapsed_activity_lines(self);
            let summaries = collapsed.iter().map(line_text).collect();
            self.work_log.push(CompletedWork {
                turn: self.turn_count,
                summaries,
                lines,
                expanded: true,
            });
            if self.work_log.len() > 100 {
                self.work_log.remove(0);
            }
        }
        self.thinking_log.clear();
        self.activity_tools.clear();
        self.pending_calls.clear();
        self.split_tool = None;
        self.split_focused = false;
        self.split_scroll = 0;
    }

    /// A new model phase can only begin after the previous batch of tools
    /// has settled. Commit that batch before accepting new deltas so the
    /// transcript retains the event stream's chronology.
    fn commit_settled_tools(&mut self, width: usize) {
        if !self.activity_tools.is_empty() && self.pending_calls.is_empty() {
            self.commit_activity(width);
        }
    }

    /// Whether the post-scroll selection hint is still within its window.
    fn scroll_hint_live(&self) -> bool {
        self.scroll_hint_at
            .is_some_and(|at| at.elapsed() < SCROLL_HINT)
    }

    /// Move pending lines into the transcript. A reader who has scrolled
    /// up stays anchored; the bottom follows new content otherwise.
    fn absorb_pending(&mut self) {
        if self.pending_history.is_empty() {
            return;
        }
        self.transcript.append(&mut self.pending_history);
        if self.transcript.len() > TRANSCRIPT_CAP {
            let excess = self.transcript.len() - TRANSCRIPT_CAP;
            self.transcript.drain(..excess);
        }
    }
}

pub async fn run(
    cfg: TuiConfig,
    worker: mpsc::UnboundedSender<WorkerCmd>,
    mut ui_rx: mpsc::UnboundedReceiver<UiMsg>,
) -> io::Result<()> {
    enable_raw_mode()?;
    // Mouse capture is on for the whole session: without it the terminal
    // never forwards the wheel to a fullscreen app. Drag-select survives it
    // — terminals keep a modifier drag (option on macOS, shift elsewhere)
    // for their own selection while an app holds the mouse.
    // Bracketed paste arrives with it: without it a multi-line paste is
    // delivered as ordinary key events, so every newline reads as enter
    // and each pasted line is submitted or queued as its own prompt.
    crossterm::execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
        default_panic(info);
    }));

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(cfg);
    let mut input = CtEventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    let shutdown = crate::shutdown_signal();
    tokio::pin!(shutdown);

    while !app.quit {
        let width = terminal.size()?.width as usize;
        app.absorb_pending();
        terminal.draw(|frame| draw(frame, &mut app))?;

        tokio::select! {
            maybe_key = input.next() => {
                match maybe_key {
                    Some(Ok(event)) => handle_terminal_event(&mut app, event, &worker, width),
                    // A dead input stream (stdin closed, terminal gone)
                    // would otherwise make this select spin forever.
                    Some(Err(_)) | None => app.quit = true,
                }
            }
            maybe_msg = ui_rx.recv() => {
                match maybe_msg {
                    Some(msg) => {
                        let content_width = transcript_content_width(&app, width);
                        handle_ui_msg(&mut app, msg, &worker, content_width)
                    },
                    None => app.quit = true,
                }
            }
            // Also tick while background processes live so their count
            // stays fresh in the status line between runs, and while the
            // scroll hint is up so it expires on its own rather than
            // waiting for whatever the reader happens to press next.
            _ = ticker.tick(), if app.running()
                || app.cfg.stats.processes() > 0
                || app.scroll_hint_live() => {
                app.spinner_frame = app.spinner_frame.wrapping_add(1);
            }
            // SIGTERM/SIGHUP: leave through the normal quit path so tool
            // destructors kill the child process groups. The guard keeps
            // the completed future from being polled again.
            _ = &mut shutdown, if !app.quit => {
                app.quit = true;
            }
        }

        // Between frames, never inside `draw`: these sequences produce no
        // cells, so a renderer interleaving its own writes with them can
        // split one mid-payload and leave the terminal parsing garbage.
        flush_terminal_requests(&mut app)?;
    }

    // Mouse tracking is unconditional at shutdown: the cost of a
    // redundant disable is nothing and the cost of a missed one is a
    // shell that reports mouse events forever.
    crossterm::execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    disable_raw_mode()?;
    println!(
        "orcacode · session ended · tokens in {} out {}",
        app.tokens_in, app.tokens_out
    );
    Ok(())
}

/// Apply the terminal-level requests the event handlers queued: a
/// clipboard write. Kept out of the handlers so those stay pure and
/// testable without a terminal; this is the only place that talks to one.
fn flush_terminal_requests(app: &mut App) -> io::Result<()> {
    let mut out = io::stdout();
    if let Some(text) = app.clipboard_pending.take() {
        out.write_all(clipboard::osc52(&text).as_bytes())?;
        out.flush()?;
    }
    Ok(())
}

/// Move the transcript view by `delta` lines: positive scrolls back
/// through history, negative returns toward the newest line. The upper
/// bound belongs to the renderer, which is the only place that knows how
/// many wrapped rows the transcript occupies at the current width.
fn scroll_transcript(app: &mut App, delta: isize) {
    if delta >= 0 {
        app.scroll = app.scroll.saturating_add(delta as usize);
    } else {
        app.scroll = app.scroll.saturating_sub(delta.unsigned_abs());
    }
    // Scrolling back is the moment someone is looking for something to
    // copy, so it is the moment worth spending the status line on. The
    // hint expires; a permanent one would just be furniture.
    app.scroll_hint_at = Some(Instant::now());
}

/// `/copy [code|all]` — push text to the terminal's clipboard.
///
/// Native selection only ever reaches the visible viewport, so the parts
/// most worth copying — a long answer, a code block that scrolled past —
/// need a path that does not go through the mouse at all.
fn copy_command(app: &mut App, arg: &str) {
    // Mid-stream, the newest text is still in the delta buffer and has
    // not become an answer yet. Copying the previous turn's answer under
    // a notice that says "last answer" would be a quiet wrong result.
    let streaming = (!app.text.trim().is_empty()).then(|| app.text.clone());
    let (label, text) = match arg {
        "" | "last" | "answer" => match streaming {
            Some(partial) => ("answer so far", Some(partial)),
            None => ("last answer", app.last_answer.clone()),
        },
        "code" | "block" => match streaming.as_deref().and_then(clipboard::last_code_block) {
            Some(block) => ("code block so far", Some(block)),
            None => (
                "last code block",
                app.last_answer
                    .as_deref()
                    .and_then(clipboard::last_code_block),
            ),
        },
        "all" | "transcript" => ("transcript", Some(transcript_text(app))),
        "tool" | "pane" => ("inspected tool", inspected_tool_text(app)),
        other => {
            push_error(
                app,
                format!("unknown /copy target: {other} — /copy [code|all|tool]"),
            );
            return;
        }
    };
    let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
        push_error(app, format!("nothing to copy: no {label} yet"));
        return;
    };
    let bytes = text.len();
    if bytes > clipboard::MAX_COPY_BYTES {
        push_error(
            app,
            format!(
                "{label} is {}KB — past the {}KB a terminal will accept in one clipboard write",
                bytes / 1024,
                clipboard::MAX_COPY_BYTES / 1024,
            ),
        );
        return;
    }
    let lines = text.lines().count();
    let plural = if lines == 1 { "" } else { "s" };
    app.clipboard_pending = Some(text);
    push_notice(
        app,
        format!("copied {label} ({lines} line{plural}) — under tmux this needs set-clipboard on"),
    );
}

/// The inspected tool as plain text: its call line, its input, and its
/// output. In split view a native drag cannot stay inside one pane — the
/// terminal selects whole rows across both — so this is how the right
/// pane comes out on its own.
fn inspected_tool_text(app: &App) -> Option<String> {
    let tool = split_inspected_tool(app)?;
    let mut out = String::new();
    out.push_str(tool.call_line.trim());
    out.push('\n');
    if let Ok(input) = serde_json::to_string_pretty(&tool.input) {
        out.push_str("\ninput\n");
        out.push_str(&input);
        out.push('\n');
    }
    match &tool.output {
        Some(output) => {
            out.push_str("\noutput\n");
            out.push_str(inspector_text_content(output).trim_end());
            out.push('\n');
        }
        None => out.push_str("\noutput\nwaiting for result\n"),
    }
    Some(out)
}

/// The tool the inspector is showing: the selected one, else the latest.
fn split_inspected_tool(app: &App) -> Option<&ToolActivity> {
    let selected = app
        .split_tool
        .unwrap_or_else(|| app.activity_tools.len().saturating_sub(1));
    app.activity_tools
        .get(selected)
        .or_else(|| app.activity_tools.last())
        .or(app.split_snapshot.as_ref())
}

/// The transcript rendered back to plain text, styling and layout
/// dropped. Trailing blank lines from block spacing go with it; a
/// clipboard payload that ends in six empty rows is nobody's intent.
fn transcript_text(app: &App) -> String {
    let mut lines: Vec<String> = app
        .transcript
        .iter()
        .chain(app.pending_history.iter())
        .map(|line| line_text(line).trim_end().to_string())
        .collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn handle_terminal_event(
    app: &mut App,
    event: CtEvent,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    width: usize,
) {
    let key = match event {
        CtEvent::Key(key) => key,
        CtEvent::Mouse(mouse) => {
            let split_boundary = ((width as u32 * 58) / 100) as u16;
            let over_inspector =
                app.view_mode == ViewMode::Split && width >= 100 && mouse.column >= split_boundary;
            match mouse.kind {
                MouseEventKind::ScrollUp if over_inspector => {
                    app.split_scroll = app.split_scroll.saturating_sub(3)
                }
                MouseEventKind::ScrollDown if over_inspector => {
                    app.split_scroll = app.split_scroll.saturating_add(3)
                }
                // An open palette is a list, not part of the transcript:
                // the wheel moves its selection rather than scrolling
                // the conversation out from under it.
                MouseEventKind::ScrollUp if app.palette_query().is_some() => palette_move(app, -1),
                MouseEventKind::ScrollDown if app.palette_query().is_some() => palette_move(app, 1),
                MouseEventKind::ScrollUp => scroll_transcript(app, 3),
                MouseEventKind::ScrollDown => scroll_transcript(app, -3),
                _ => {}
            }
            return;
        }
        // A paste is composer input only: an open approval or overlay is
        // a keystroke menu with nowhere to put the text.
        CtEvent::Paste(text) => {
            if app.approval.is_none() && app.overlay.is_none() {
                insert_paste(app, &text);
            }
            return;
        }
        _ => return,
    };
    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
        return;
    }
    if app.approval.is_some() {
        handle_approval_key(app, key);
        return;
    }
    if app.overlay.is_some() {
        handle_overlay_key(app, key, worker);
        return;
    }
    let content_width = transcript_content_width(app, width);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char('c') if ctrl => {
            if let RunState::Running { cancel, .. } = &app.run {
                cancel.cancel();
            } else if !app.composer.is_empty() {
                app.composer.clear();
                app.cursor = 0;
            } else {
                app.quit = true;
            }
        }
        KeyCode::Char('d') if ctrl && app.composer.is_empty() => app.quit = true,
        KeyCode::Char('a') if ctrl => app.cursor = 0,
        KeyCode::Char('e') if ctrl => app.cursor = app.composer.chars().count(),
        KeyCode::Char('u') if ctrl => {
            app.composer.clear();
            app.cursor = 0;
        }
        KeyCode::Char('o') if ctrl => {
            if !expand_latest_work(app) {
                expand_tool(app, 1, content_width);
            }
        }
        // Deliberately not ctrl+s (XOFF on most terminals — the app would
        // appear to hang) and not a plain letter the composer needs.
        // ctrl+y copies whichever pane has focus, so the same key means
        // "take what I am looking at" in either half of a split.
        KeyCode::Char('y') if ctrl => {
            copy_command(app, if app.split_focused { "tool" } else { "" })
        }
        // Line scroll on shift+arrows: PgUp/PgDn are fn+arrows on a laptop
        // keyboard, which terminals often swallow for their own scrollback
        // before the app ever sees them.
        KeyCode::Up if shift => scroll_transcript(app, 1),
        KeyCode::Down if shift => scroll_transcript(app, -1),
        KeyCode::Tab
            if app.view_mode == ViewMode::Split
                && width >= 100
                && !app.activity_tools.is_empty() =>
        {
            app.split_focused = !app.split_focused;
            if app.split_tool.is_none() {
                app.split_tool = Some(app.activity_tools.len() - 1);
            }
        }
        KeyCode::Up if app.split_focused => {
            let selected = app
                .split_tool
                .unwrap_or_else(|| app.activity_tools.len().saturating_sub(1));
            app.split_tool = Some(selected.saturating_sub(1));
            app.split_scroll = 0;
        }
        KeyCode::Down if app.split_focused => {
            let last = app.activity_tools.len().saturating_sub(1);
            app.split_tool = Some(app.split_tool.unwrap_or(last).saturating_add(1).min(last));
            app.split_scroll = 0;
        }
        KeyCode::PageUp if app.split_focused => {
            app.split_scroll = app.split_scroll.saturating_sub(SCROLL_PAGE as u16);
        }
        KeyCode::PageDown if app.split_focused => {
            app.split_scroll = app.split_scroll.saturating_add(SCROLL_PAGE as u16);
        }
        // Page keys belong to the palette while it is open, for the same
        // reason the wheel does: the list is what the user is looking at.
        KeyCode::PageUp if app.palette_query().is_some() => {
            palette_move(app, -(PALETTE_ROWS as isize))
        }
        KeyCode::PageDown if app.palette_query().is_some() => {
            palette_move(app, PALETTE_ROWS as isize)
        }
        KeyCode::PageUp => scroll_transcript(app, SCROLL_PAGE as isize),
        KeyCode::PageDown => scroll_transcript(app, -(SCROLL_PAGE as isize)),
        KeyCode::Esc => {
            if app.split_focused {
                app.split_focused = false;
            } else if app.palette_query().is_some() {
                app.composer.clear();
                app.cursor = 0;
                app.palette_index = 0;
            } else if let RunState::Running { cancel, .. } = &app.run {
                cancel.cancel();
            } else {
                app.composer.clear();
                app.cursor = 0;
            }
        }
        KeyCode::Tab => {
            if let Some(spec) = palette_selection(app) {
                app.composer = if spec.takes_args {
                    format!("/{} ", spec.name)
                } else {
                    format!("/{}", spec.name)
                };
                app.cursor = app.composer.chars().count();
            }
        }
        KeyCode::Enter => {
            // Palette open with a selection and no arguments typed:
            // enter uses the highlighted command as-is.
            if let Some(spec) = palette_selection(app) {
                if !app.composer.trim().contains(char::is_whitespace) {
                    app.composer = format!("/{}", spec.name);
                    app.cursor = app.composer.chars().count();
                }
            }
            app.scroll = 0;
            submit(app, worker, content_width);
            app.palette_index = 0;
        }
        KeyCode::Char(c) => {
            let at = byte_index(&app.composer, app.cursor);
            app.composer.insert(at, c);
            app.cursor += 1;
            app.palette_index = 0;
            if c == '@' && mention_starts_at(&app.composer, app.cursor - 1) {
                let entries = workspace_locations(Path::new(&app.cfg.workspace_root));
                app.overlay = Some(Overlay::Locations(LocationPicker {
                    picker: ListPicker::new(entries.len()),
                    entries,
                    query: String::new(),
                    token_start: app.cursor - 1,
                }));
            }
        }
        KeyCode::Backspace => {
            if let Some((start, end)) = marker_ending_at(&app.pastes, &app.composer, app.cursor) {
                remove_marker(app, start, end);
                app.palette_index = 0;
            } else if remove_location_mention_before_cursor(&mut app.composer, &mut app.cursor) {
                app.palette_index = 0;
            } else if app.cursor > 0 {
                let at = byte_index(&app.composer, app.cursor - 1);
                app.composer.remove(at);
                app.cursor -= 1;
                app.palette_index = 0;
            }
        }
        KeyCode::Delete => {
            if let Some((start, end)) = marker_starting_at(&app.pastes, &app.composer, app.cursor) {
                remove_marker(app, start, end);
                app.palette_index = 0;
            } else if app.cursor < app.composer.chars().count() {
                let at = byte_index(&app.composer, app.cursor);
                app.composer.remove(at);
                app.palette_index = 0;
            }
        }
        // Arrows step over a marker whole too: landing inside one would
        // let the next keystroke corrupt it into text that no longer
        // expands.
        KeyCode::Left => {
            app.cursor = match marker_ending_at(&app.pastes, &app.composer, app.cursor) {
                Some((start, _)) => start,
                None => app.cursor.saturating_sub(1),
            }
        }
        KeyCode::Right => {
            app.cursor = match marker_starting_at(&app.pastes, &app.composer, app.cursor) {
                Some((_, end)) => end,
                None => (app.cursor + 1).min(app.composer.chars().count()),
            }
        }
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.composer.chars().count(),
        KeyCode::Up => {
            if app.palette_query().is_some() {
                palette_move(app, -1);
            } else {
                history_nav(app, -1);
            }
        }
        KeyCode::Down => {
            if app.palette_query().is_some() {
                palette_move(app, 1);
            } else {
                history_nav(app, 1);
            }
        }
        _ => {}
    }
}

/// Keys while a modal overlay is open: navigation, live filtering, and
/// selection. The overlay swallows everything except quit chords.
fn handle_overlay_key(app: &mut App, key: KeyEvent, worker: &mpsc::UnboundedSender<WorkerCmd>) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c')) {
        app.overlay = None;
        return;
    }
    // In-place edits happen under the borrow; anything that replaces the
    // overlay or talks to the worker is deferred until the borrow ends.
    enum After {
        Nothing,
        Close,
        Replace(Overlay),
        /// Send to the worker with the overlay left open (toggle rows).
        Send(WorkerCmd),
        CloseAndSend(WorkerCmd),
        /// Close, remember the picked model's context window, switch model.
        CloseAndSetModel {
            id: String,
            window: Option<u64>,
        },
        /// Close, persist, and activate the selected transcript layout.
        CloseAndSetView(ViewMode),
        /// Close and drop a dim status line into the history.
        CloseWithNote(String),
        /// Keep the overlay open and drop a dim status line (row
        /// actions that mutate the list in place).
        Note(String),
        /// Close, send to the worker, and drop a dim status line.
        SendWithNote(WorkerCmd, String),
        /// Close and start the /models fetch-then-pick flow.
        FetchModels,
        /// Delete an installed skill, then rescan. Deferred out of the
        /// overlay match because it needs `app` (the shared handle, the
        /// config, the transcript), which the match holds borrowed.
        RemoveSkill(String),
        /// Close and replace the active `@query` with a workspace path.
        InsertLocation {
            token_start: usize,
            entry: LocationEntry,
        },
    }
    // Read before the overlay borrow: the settings rows need these.
    let current_provider = app.cfg.provider;
    let current_view = app.view_mode;
    let workspace_root = app.cfg.workspace_root.clone();
    let current_session = app.cfg.session_id.clone();
    let Some(overlay) = app.overlay.as_mut() else {
        return;
    };
    let after = match overlay {
        Overlay::Models(picker) => {
            let after = match key.code {
                KeyCode::Up => {
                    picker.index = picker.index.saturating_sub(1);
                    After::Nothing
                }
                KeyCode::Down => {
                    picker.index += 1;
                    After::Nothing
                }
                KeyCode::PageUp => {
                    picker.index = picker.index.saturating_sub(PICKER_ROWS);
                    After::Nothing
                }
                KeyCode::PageDown => {
                    picker.index += PICKER_ROWS;
                    After::Nothing
                }
                KeyCode::Enter => match picker.selected_info() {
                    Some((id, window)) => After::CloseAndSetModel { id, window },
                    None => After::Close,
                },
                KeyCode::Char(c) => {
                    picker.filter.push(c);
                    picker.index = 0;
                    After::Nothing
                }
                KeyCode::Backspace => {
                    picker.filter.pop();
                    picker.index = 0;
                    After::Nothing
                }
                _ => After::Nothing,
            };
            let len = picker.filtered().len();
            picker.index = picker.index.min(len.saturating_sub(1));
            after
        }
        Overlay::Locations(location) => match key.code {
            KeyCode::Tab => match location.selected() {
                Some(entry) => After::InsertLocation {
                    token_start: location.token_start,
                    entry,
                },
                None => After::Nothing,
            },
            _ => match location.picker.on_key(key.code) {
                PickerEvent::Activated(_) => match location.selected() {
                    Some(entry) => After::InsertLocation {
                        token_start: location.token_start,
                        entry,
                    },
                    None => After::Nothing,
                },
                PickerEvent::Moved | PickerEvent::Action { .. } => After::Nothing,
                PickerEvent::Ignored => match key.code {
                    KeyCode::Char(c) => {
                        location.query.push(c);
                        let at = byte_index(&app.composer, app.cursor);
                        app.composer.insert(at, c);
                        app.cursor += 1;
                        location.sync_len();
                        After::Nothing
                    }
                    KeyCode::Backspace if !location.query.is_empty() => {
                        location.query.pop();
                        let at = byte_index(&app.composer, app.cursor - 1);
                        app.composer.remove(at);
                        app.cursor -= 1;
                        location.sync_len();
                        After::Nothing
                    }
                    KeyCode::Backspace | KeyCode::Delete => {
                        let at = byte_index(&app.composer, location.token_start);
                        app.composer.remove(at);
                        app.cursor = location.token_start;
                        After::Close
                    }
                    _ => After::Nothing,
                },
            },
        },
        Overlay::Providers { picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => {
                let provider = Provider::ALL[index];
                if provider.key_env().is_some() && provider.resolve_key().is_none() {
                    // No key in the shell or config file: ask before switching.
                    After::Replace(Overlay::ApiKey {
                        provider,
                        input: String::new(),
                    })
                } else {
                    After::CloseAndSend(WorkerCmd::SetProvider {
                        provider,
                        api_key: None,
                    })
                }
            }
            _ => After::Nothing,
        },
        Overlay::Themes { picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => {
                let name = view::ThemeName::ALL[index];
                view::set_theme(name);
                let note = match crate::config::save_theme(name.slug()) {
                    Ok(_) => format!("theme set to {}", name.label()),
                    Err(err) => format!("theme set to {} (not saved: {err})", name.label()),
                };
                After::CloseWithNote(note)
            }
            _ => After::Nothing,
        },
        Overlay::Views { picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => After::CloseAndSetView(ViewMode::ALL[index]),
            _ => After::Nothing,
        },
        Overlay::Usage => match key.code {
            KeyCode::Enter | KeyCode::Char('q') => After::Close,
            _ => After::Nothing,
        },
        Overlay::ApiKey { provider, input } => match key.code {
            KeyCode::Enter => {
                let key = input.trim().to_string();
                if key.is_empty() {
                    After::Nothing
                } else {
                    let provider = *provider;
                    let note = match crate::config::save_key(provider.label(), &key) {
                        Ok(path) => format!("api key saved to {}", path.display()),
                        Err(err) => {
                            format!("api key kept for this session only (save failed: {err})")
                        }
                    };
                    After::SendWithNote(
                        WorkerCmd::SetProvider {
                            provider,
                            api_key: Some(key),
                        },
                        note,
                    )
                }
            }
            KeyCode::Char(c) => {
                input.push(c);
                After::Nothing
            }
            KeyCode::Backspace => {
                input.pop();
                After::Nothing
            }
            _ => After::Nothing,
        },
        Overlay::Settings { picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(row) => match row {
                0 => {
                    let selected = Provider::ALL
                        .iter()
                        .position(|p| *p == current_provider)
                        .unwrap_or(0);
                    After::Replace(Overlay::Providers {
                        picker: ListPicker::with_selected(Provider::ALL.len(), selected),
                    })
                }
                1 => After::FetchModels,
                2 => {
                    let current = view::theme_name();
                    let selected = view::ThemeName::ALL
                        .iter()
                        .position(|name| *name == current)
                        .unwrap_or(0);
                    After::Replace(Overlay::Themes {
                        picker: ListPicker::with_selected(view::ThemeName::ALL.len(), selected),
                    })
                }
                3 => {
                    let selected = ViewMode::ALL
                        .iter()
                        .position(|mode| *mode == current_view)
                        .unwrap_or(0);
                    After::Replace(Overlay::Views {
                        picker: ListPicker::with_selected(ViewMode::ALL.len(), selected),
                    })
                }
                4 => {
                    if current_provider.key_env().is_none() {
                        After::CloseWithNote(format!(
                            "the {} endpoint needs no api key",
                            current_provider.label()
                        ))
                    } else {
                        After::Replace(Overlay::ApiKey {
                            provider: current_provider,
                            input: String::new(),
                        })
                    }
                }
                _ => {
                    let tools = crate::config::stored_approvals(&workspace_root);
                    if tools.is_empty() {
                        After::CloseWithNote("no saved approvals for this workspace".into())
                    } else {
                        After::Replace(Overlay::Approvals {
                            picker: ListPicker::new(tools.len()),
                            tools,
                        })
                    }
                }
            },
            _ => After::Nothing,
        },
        Overlay::Approvals { tools, picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => {
                let tool = tools[index].clone();
                match crate::config::remove_approval(&workspace_root, &tool) {
                    Ok(_) => {
                        tools.retain(|t| *t != tool);
                        picker.set_len(tools.len());
                        if tools.is_empty() {
                            After::CloseWithNote(
                                "all saved approvals removed; these tools ask again".into(),
                            )
                        } else {
                            After::Nothing
                        }
                    }
                    Err(err) => After::CloseWithNote(format!("could not update the config: {err}")),
                }
            }
            _ => After::Nothing,
        },
        Overlay::Extensions { picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => {
                let spec = &crate::extensions::EXTENSIONS[index];
                let enabled = !crate::extensions::is_enabled(spec);
                match crate::config::save_extension(spec.name, enabled) {
                    // Stay open so several extensions can be toggled;
                    // the row re-renders with its new state.
                    Ok(_) => After::Send(WorkerCmd::ReloadExtensions),
                    Err(err) => After::CloseWithNote(format!("could not update the config: {err}")),
                }
            }
            _ => After::Nothing,
        },
        Overlay::Mcp {
            servers,
            filter,
            picker,
        } => {
            let indices = matching_indices(servers, filter, |server| &server.name);
            // Space toggles rather than arming an action strip: this
            // picker has exactly one action, so the strip would be a
            // keystroke of ceremony. Enter does the same, matching
            // /extensions.
            let row = match key.code {
                KeyCode::Char(' ') | KeyCode::Enter if !indices.is_empty() => {
                    Some(indices[picker.index()])
                }
                _ => match picker.on_key(key.code) {
                    PickerEvent::Activated(index) => Some(indices[index]),
                    PickerEvent::Ignored => {
                        match key.code {
                            KeyCode::Char(c) if c != ' ' => filter.push(c),
                            KeyCode::Backspace => {
                                filter.pop();
                            }
                            _ => {}
                        }
                        picker.set_len(
                            matching_indices(servers, filter, |server| &server.name).len(),
                        );
                        return;
                    }
                    _ => None,
                },
            };
            match row {
                Some(index) => {
                    let server = &mut servers[index];
                    let enabled = !server.enabled;
                    match crate::config::set_mcp_enabled(&server.name, enabled) {
                        // Stay open so several servers can be toggled;
                        // the row redraws from this copy at once while
                        // the reconnect runs behind the overlay.
                        Ok(_) => {
                            server.enabled = enabled;
                            After::Send(WorkerCmd::ReloadMcp)
                        }
                        Err(err) => {
                            After::CloseWithNote(format!("could not update the config: {err}"))
                        }
                    }
                }
                None => After::Nothing,
            }
        }
        Overlay::Skills {
            entries,
            filter,
            picker,
        } => match picker.on_key(key.code) {
            // Space reveals the action strip; enter keeps the toggle one
            // key away, since that is what the list is mostly for.
            // Deleting sits behind the strip on purpose: it is the only
            // action here that touches the filesystem. It also needs
            // `app` — the shared handle, the config, the transcript —
            // which this match holds borrowed, so it is handed to the
            // apply step below.
            PickerEvent::Action { key: 'd', row } => {
                let indices = matching_indices(entries, filter, |entry| &entry.name);
                After::RemoveSkill(entries[indices[row]].name.clone())
            }
            event => {
                let row = match event {
                    PickerEvent::Activated(index)
                    | PickerEvent::Action {
                        key: 't',
                        row: index,
                    } => {
                        let indices = matching_indices(entries, filter, |entry| &entry.name);
                        Some(indices[index])
                    }
                    PickerEvent::Ignored => {
                        match key.code {
                            KeyCode::Char(c) if c != ' ' => filter.push(c),
                            KeyCode::Backspace => {
                                filter.pop();
                            }
                            _ => {}
                        }
                        picker
                            .set_len(matching_indices(entries, filter, |entry| &entry.name).len());
                        None
                    }
                    _ => None,
                };
                match row {
                    Some(index) => {
                        let entry = &mut entries[index];
                        match &entry.state {
                            // A row that never loaded has nothing to
                            // switch on; saying why beats a toggle that
                            // does nothing.
                            crate::skills::SkillState::Failed { reason, .. } => {
                                After::Note(format!("skill {} did not load: {reason}", entry.name))
                            }
                            crate::skills::SkillState::Shadowed { root, by } => {
                                After::Note(format!(
                                    "skill {} in {root} is shadowed by the copy in {by} — \
                                     rename it to use both",
                                    entry.name
                                ))
                            }
                            crate::skills::SkillState::Loaded { .. } => {
                                let enabled = !entry.enabled;
                                match crate::config::save_skill_enabled(&entry.name, enabled) {
                                    // Stay open so several can be
                                    // toggled; the row redraws from this
                                    // copy at once while the rescan runs
                                    // behind it.
                                    Ok(_) => {
                                        entry.enabled = enabled;
                                        After::Send(WorkerCmd::ReloadSkills)
                                    }
                                    Err(err) => After::CloseWithNote(format!(
                                        "could not update the config: {err}"
                                    )),
                                }
                            }
                        }
                    }
                    None => After::Nothing,
                }
            }
        },
        Overlay::Sessions { sessions, picker } => {
            // The picker only shows the last few sessions, but the
            // navigation grammar matches /models: ↑↓ step, PgUp/PgDn
            // page through the list.
            match key.code {
                KeyCode::PageUp => {
                    picker.move_by(-(PICKER_ROWS as isize));
                    After::Nothing
                }
                KeyCode::PageDown => {
                    picker.move_by(PICKER_ROWS as isize);
                    After::Nothing
                }
                _ => match picker.on_key(key.code) {
                    PickerEvent::Activated(index) => After::CloseAndSend(WorkerCmd::LoadSession {
                        path: sessions[index].path.clone(),
                    }),
                    PickerEvent::Action { key: 'd', row } => {
                        let session = &sessions[row];
                        if current_session.as_deref() == Some(session.meta.id.as_str()) {
                            After::Note(
                                "the active session cannot be deleted (use /clear to empty it)"
                                    .into(),
                            )
                        } else {
                            match std::fs::remove_file(&session.path) {
                                Ok(()) => {
                                    let id = sessions.remove(row).meta.id;
                                    picker.set_len(sessions.len());
                                    if sessions.is_empty() {
                                        After::CloseWithNote(format!("deleted session {id}"))
                                    } else {
                                        After::Note(format!("deleted session {id}"))
                                    }
                                }
                                Err(err) => After::Note(format!("could not delete: {err}")),
                            }
                        }
                    }
                    _ => After::Nothing,
                },
            }
        }
    };
    match after {
        After::Nothing => {}
        After::Close => app.overlay = None,
        After::Replace(next) => app.overlay = Some(next),
        After::Send(cmd) => send_or_report(app, worker, cmd),
        After::CloseAndSend(cmd) => {
            app.overlay = None;
            send_or_report(app, worker, cmd);
        }
        After::CloseAndSetModel { id, window } => {
            app.overlay = None;
            app.context_window = window;
            send_or_report(app, worker, WorkerCmd::SetModel { id });
        }
        After::CloseAndSetView(mode) => {
            app.overlay = None;
            app.view_mode = mode;
            if mode == ViewMode::Classic {
                clear_tool_connectors(&mut app.transcript);
                clear_tool_connectors(&mut app.pending_history);
                app.split_inspector_cache = None;
            }
            app.split_focused = false;
            app.split_scroll = 0;
            let note = match crate::config::save_view(mode.slug()) {
                Ok(_) => format!("view set to {}", mode.label()),
                Err(err) => format!("view set to {} (not saved: {err})", mode.label()),
            };
            push_notice(app, note);
        }
        After::CloseWithNote(note) => {
            app.overlay = None;
            push_notice(app, note);
        }
        After::Note(note) => {
            push_notice(app, note);
        }
        After::SendWithNote(cmd, note) => {
            app.overlay = None;
            send_or_report(app, worker, cmd);
            push_notice(app, note);
        }
        After::RemoveSkill(name) => {
            // The rescan that follows is the worker's, and it lands
            // later; drop the row now so the list matches what the user
            // just did. An unremovable skill keeps its row and says why.
            if remove_skill(app, &name, worker) {
                if let Some(Overlay::Skills {
                    entries,
                    filter,
                    picker,
                }) = &mut app.overlay
                {
                    entries.retain(|entry| entry.name != name);
                    picker.set_len(matching_indices(entries, filter, |entry| &entry.name).len());
                    if entries.is_empty() {
                        app.overlay = None;
                    }
                }
            }
        }
        After::FetchModels => {
            app.overlay = None;
            app.picker_pending = Some(String::new());
            if worker
                .send(WorkerCmd::ListModels {
                    filter: String::new(),
                })
                .is_err()
            {
                app.picker_pending = None;
                push_error(app, "worker is gone; restart orcacode");
            } else {
                push_notice(app, "fetching models…");
            }
        }
        After::InsertLocation { token_start, entry } => {
            let start_byte = byte_index(&app.composer, token_start);
            let end_byte = byte_index(&app.composer, app.cursor);
            let suffix = if entry.directory { "/" } else { "" };
            let mention = format!("@{}{suffix} ", entry.path);
            app.composer.replace_range(start_byte..end_byte, &mention);
            app.cursor = token_start + mention.chars().count();
            app.overlay = None;
        }
    }
}

fn send_or_report(app: &mut App, worker: &mpsc::UnboundedSender<WorkerCmd>, cmd: WorkerCmd) {
    if worker.send(cmd).is_err() {
        push_error(app, "worker is gone; restart orcacode");
    }
}

/// Move the palette selection by `delta` rows, clamped to the filtered
/// list. Arrow keys, page keys, and the wheel all go through here so
/// they cannot disagree about the bounds; the rendered window follows
/// the selection, so this is what scrolling the list means.
fn palette_move(app: &mut App, delta: isize) {
    let Some(query) = app.palette_query() else {
        return;
    };
    let Some(last) = filter_commands(query).len().checked_sub(1) else {
        return;
    };
    let current = app.palette_index.min(last) as isize;
    app.palette_index = current.saturating_add(delta).clamp(0, last as isize) as usize;
}

/// The highlighted palette entry, if the palette is open and non-empty.
fn palette_selection(app: &App) -> Option<&'static CommandSpec> {
    let query = app.palette_query()?;
    let filtered = filter_commands(query);
    filtered
        .get(app.palette_index.min(filtered.len().saturating_sub(1)))
        .copied()
}

/// A compact transcript notification: the dot keeps system status scannable
/// without giving it the visual weight of transcript content.
fn push_notice(app: &mut App, text: impl Into<String>) {
    let t = theme();
    app.push_line(Line::from(vec![
        Span::styled("• ", t.accent),
        Span::styled(text.into(), t.dim),
    ]));
}

/// A command that refused to do what was asked. The same leading glyph as
/// [`push_notice`] — a failed command is still the system talking, and a
/// line without the glyph reads as model output — with the body in the
/// error color so severity and origin are two separate cues.
fn push_error(app: &mut App, text: impl Into<String>) {
    let t = theme();
    app.push_line(Line::from(vec![
        Span::styled("• ", t.accent),
        Span::styled(text.into(), t.error),
    ]));
}

fn handle_approval_key(app: &mut App, key: KeyEvent) {
    let response = match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(ApprovalResponse::AllowOnce),
        KeyCode::Char('a') => Some(ApprovalResponse::AllowAlways),
        // Deliberately a distinct key: persisting trust across sessions
        // must never happen from a habitual lowercase 'a'.
        KeyCode::Char('A') => Some(ApprovalResponse::AllowAlwaysSave),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(ApprovalResponse::Deny),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(ApprovalResponse::Deny)
        }
        _ => None,
    };
    if let Some(response) = response {
        if let Some(request) = app.approval.take() {
            let verdict = match response {
                ApprovalResponse::AllowOnce => "approved",
                ApprovalResponse::AllowAlways => "always allowed",
                ApprovalResponse::AllowAlwaysSave => "always allowed (saved)",
                ApprovalResponse::Deny => "denied",
            };
            if response == ApprovalResponse::AllowAlwaysSave {
                let note =
                    match crate::config::save_approval(&app.cfg.workspace_root, &request.tool_name)
                    {
                        Ok(_) => format!(
                            "{} always allowed in this workspace — saved; /settings to revoke",
                            request.tool_name
                        ),
                        Err(err) => format!(
                            "{} always allowed this session only (save failed: {err})",
                            request.tool_name
                        ),
                    };
                push_notice(app, note);
            }
            if let Some(activity) = app.activity_tools.iter_mut().rev().find(|activity| {
                activity.tool_name == request.tool_name && activity.output.is_none()
            }) {
                activity.approval = Some(verdict.to_string());
            }
            let _ = request.respond.send(response);
        }
    }
}

fn history_nav(app: &mut App, dir: i32) {
    if app.prompt_history.is_empty() {
        return;
    }
    let last = app.prompt_history.len() - 1;
    let next = match (app.history_pos, dir) {
        (None, -1) => Some(last),
        (None, _) => None,
        (Some(0), -1) => Some(0),
        (Some(p), -1) => Some(p - 1),
        (Some(p), 1) if p >= last => None,
        (Some(p), 1) => Some(p + 1),
        (pos, _) => pos,
    };
    app.history_pos = next;
    app.composer = next
        .map(|p| app.prompt_history[p].clone())
        .unwrap_or_default();
    app.cursor = app.composer.chars().count();
}

fn submit(app: &mut App, worker: &mpsc::UnboundedSender<WorkerCmd>, width: usize) {
    let prompt = app.composer.trim().to_string();
    if prompt.is_empty() {
        if !app.running() {
            start_next_queued_prompt(app, worker, width);
        }
        return;
    }

    // Queue management is intentionally available during a run. Other
    // slash commands retain the existing one-run-at-a-time behavior and
    // stay in the composer until the active turn finishes.
    let command = prompt.strip_prefix('/').map(str::trim);
    if app.running()
        && !command.is_some_and(|command| command == "queue" || command.starts_with("queue "))
        && command.is_some()
    {
        return;
    }

    app.composer.clear();
    app.cursor = 0;
    app.history_pos = None;
    app.prompt_history.push(prompt.clone());

    if let Some(command) = command {
        slash_command(app, command, worker, width);
        return;
    }

    if app.running() || !app.prompt_queue.is_empty() {
        app.prompt_queue.push_back(prompt);
        if !app.running() {
            start_next_queued_prompt(app, worker, width);
        }
        return;
    }

    start_submission(app, worker, prompt, width);
}

fn start_submission(
    app: &mut App,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    prompt: String,
    width: usize,
) -> bool {
    if let Some(command) = prompt.strip_prefix('!') {
        let command = expand_pastes(&app.pastes, command.trim());
        start_shell(
            app,
            worker,
            expand_pastes(&app.pastes, &prompt),
            // The picker also fires on `!` lines, and a shell has no use for
            // the marker either.
            strip_location_mentions(command.trim()),
            width,
        )
    } else {
        start_prompt(app, worker, prompt, width)
    }
}

fn start_shell(
    app: &mut App,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    prompt: String,
    command: String,
    width: usize,
) -> bool {
    if command.is_empty() {
        push_notice(app, "usage: !command");
        return false;
    }
    let cancel = CancellationToken::new();
    if worker
        .send(WorkerCmd::Shell {
            command,
            working_dir: app.cfg.workspace_root.clone(),
            cancel: cancel.clone(),
        })
        .is_err()
    {
        push_error(app, "worker is gone; restart orcacode");
        return false;
    }
    app.reset_activity();
    if app.turn_count > 0 {
        app.push_line(Line::from(""));
    }
    app.push_wrapped(&prompt, "┃ ", theme().strong, width);
    app.turn_count += 1;
    app.run = RunState::Running {
        started: Instant::now(),
        cancel,
    };
    true
}

/// Start one prompt and commit it to the transcript only after the worker
/// accepts it. Returns false when the worker is gone.
fn start_prompt(
    app: &mut App,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    prompt: String,
    width: usize,
) -> bool {
    let cancel = CancellationToken::new();
    // The marker is a composer affordance only. Once sent, the turn
    // shows what the model actually received.
    let prompt = expand_pastes(&app.pastes, &prompt);
    app.context_tokens += (prompt.len() / 4) as u64;
    if worker
        .send(WorkerCmd::Run {
            prompt: strip_location_mentions(&prompt),
            cancel: cancel.clone(),
        })
        .is_err()
    {
        push_error(app, "worker is gone; restart orcacode");
        return false;
    }

    app.reset_activity();

    if app.turn_count > 0 {
        app.push_line(Line::from(""));
    }
    app.push_wrapped(&prompt, "┃ ", theme().strong, width);
    app.turn_count += 1;
    app.run = RunState::Running {
        started: Instant::now(),
        cancel,
    };
    true
}

/// Resume the oldest waiting prompt. A failed send leaves the item queued
/// so the UI cannot silently discard work.
fn start_next_queued_prompt(
    app: &mut App,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    width: usize,
) -> bool {
    let Some(prompt) = app.prompt_queue.front().cloned() else {
        return false;
    };
    if !start_submission(app, worker, prompt, width) {
        return false;
    }
    app.prompt_queue.pop_front();
    true
}

/// Print the full output of the n-th most recent tool call (1 = latest)
/// into the transcript.
fn expand_tool(app: &mut App, nth_latest: usize, width: usize) {
    let t = theme();
    let Some(record) = app.tool_log.iter().rev().nth(nth_latest.saturating_sub(1)) else {
        push_notice(app, "nothing to expand");
        return;
    };
    let lines = view::expand_output(&record.tool_name, &record.output);
    let mut rendered = Vec::new();
    rendered.push(Line::from(vec![
        Span::styled("  ┌ ", t.dim),
        Span::styled(record.call_line.clone(), t.accent),
    ]));
    let body_width = width.saturating_sub(6).max(16);
    for line in lines.iter().take(EXPAND_MAX_LINES) {
        rendered.push(Line::from(vec![
            Span::styled("  │ ", t.dim),
            Span::raw(view::truncate_line(line, body_width)),
        ]));
    }
    if lines.len() > EXPAND_MAX_LINES {
        rendered.push(Line::from(Span::styled(
            format!("  │ … {} more lines", lines.len() - EXPAND_MAX_LINES),
            t.dim,
        )));
    }
    if !record.inner.is_empty() {
        rendered.push(Line::from(vec![
            Span::styled("  │ ", t.dim),
            Span::styled("inner activity:", t.dim),
        ]));
        for line in &record.inner {
            rendered.push(Line::from(vec![
                Span::styled("  │   ", t.dim),
                Span::raw(view::truncate_line(line, body_width.saturating_sub(2))),
            ]));
        }
    }
    rendered.push(Line::from(Span::styled("  └", t.dim)));
    app.pending_history.extend(rendered);
}

/// Reveal the most recent completed turn's work tree. Raw output remains
/// available through `/expand n`, so this shortcut can focus on structure.
fn expand_latest_work(app: &mut App) -> bool {
    let Some(work) = app.work_log.last() else {
        return false;
    };
    if work.turn != app.turn_count {
        return true;
    }
    if work.expanded {
        return true;
    }

    let summaries = work.summaries.clone();
    let lines = work.lines.clone();
    let inserted = replace_block(&mut app.pending_history, &summaries, &lines)
        || replace_block(&mut app.transcript, &summaries, &lines);
    if inserted {
        if let Some(work) = app.work_log.last_mut() {
            work.expanded = true;
        }
    }
    true
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn clear_tool_connectors(lines: &mut [Line<'static>]) {
    for line in lines {
        line.spans.retain(|span| {
            let text = span.content.trim();
            !(text.contains('·')
                && text.ends_with('○')
                && text.chars().all(|ch| ch == '·' || ch == '○'))
        });
    }
}

fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|span| span.content.trim().is_empty())
}

fn trim_blank_edges(lines: &mut Vec<Line<'static>>) {
    let start = lines.iter().position(|line| !line_is_blank(line));
    let Some(start) = start else {
        lines.clear();
        return;
    };
    let end = lines
        .iter()
        .rposition(|line| !line_is_blank(line))
        .expect("non-empty block")
        + 1;
    lines.drain(end..);
    lines.drain(..start);
}

fn append_render_block(
    target: &mut Vec<Line<'static>>,
    mut block: Vec<Line<'static>>,
    spacing: BlockSpacing,
    fallback_prior: Option<bool>,
) -> bool {
    trim_blank_edges(&mut block);
    if block.is_empty() {
        return false;
    }
    while target.last().is_some_and(line_is_blank) {
        target.pop();
    }
    let prior = target.last().map(line_is_blank).or(fallback_prior);
    if prior.is_some() && prior == Some(false) && spacing == BlockSpacing::Section {
        target.push(Line::from(""));
    }
    target.extend(block);
    true
}

fn push_wrapped_lines(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    indent: &str,
    style: Style,
    width: usize,
) {
    let body_width = width.saturating_sub(indent.len()).max(16);
    for paragraph in text.split('\n') {
        if paragraph.trim().is_empty() {
            lines.push(Line::from(""));
            continue;
        }
        for piece in textwrap::wrap(paragraph, body_width) {
            lines.push(Line::from(Span::styled(format!("{indent}{piece}"), style)));
        }
    }
}

fn replace_block(
    lines: &mut Vec<Line<'static>>,
    targets: &[String],
    replacement: &[Line<'static>],
) -> bool {
    if targets.is_empty() || targets.len() > lines.len() {
        return false;
    }
    let Some(index) = (0..=lines.len() - targets.len()).rev().find(|start| {
        lines[*start..*start + targets.len()]
            .iter()
            .map(line_text)
            .eq(targets.iter().cloned())
    }) else {
        return false;
    };
    lines.splice(index..index + targets.len(), replacement.iter().cloned());
    true
}

fn slash_command(
    app: &mut App,
    command: &str,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    width: usize,
) {
    let dim = theme().dim;
    if command == "queue" {
        let queued = app.prompt_queue.len();
        let message = match queued {
            0 => "queue empty".to_string(),
            1 => "1 prompt queued".to_string(),
            _ => format!("{queued} prompts queued"),
        };
        app.push_line(Line::from(Span::styled(message, dim)));
        return;
    }
    if let Some(rest) = command.strip_prefix("queue ") {
        if rest.trim() == "clear" {
            let cleared = app.prompt_queue.len();
            app.prompt_queue.clear();
            let message = match cleared {
                0 => "queue already empty".to_string(),
                1 => "cleared 1 queued prompt".to_string(),
                _ => format!("cleared {cleared} queued prompts"),
            };
            app.push_line(Line::from(Span::styled(message, dim)));
            return;
        }
        push_error(app, "usage: /queue [clear]");
        return;
    }
    if let Some(rest) = command.strip_prefix("expand") {
        let nth = rest.trim().parse::<usize>().unwrap_or(1).max(1);
        expand_tool(app, nth, width);
        return;
    }
    if let Some(rest) = command.strip_prefix("mode") {
        if rest.is_empty() || rest.starts_with(' ') {
            mode_command(app, rest.trim());
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("rewind") {
        if rest.is_empty() || rest.starts_with(' ') {
            rewind_command(app, rest.trim(), worker);
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("todo") {
        if rest.is_empty() || rest.starts_with(' ') {
            todo_command(app, width);
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("subagents") {
        if rest.is_empty() {
            push_notice(
                app,
                format!("subagent nesting depth: {}", app.cfg.subagent_depth.get()),
            );
            return;
        }
        if let Some(arg) = rest.strip_prefix(' ') {
            match arg.trim().parse::<u32>() {
                Ok(depth) => {
                    let set = app.cfg.subagent_depth.set(depth);
                    app.push_line(Line::from(Span::styled(
                        format!("subagent nesting depth set to {set} (applies to the next spawn)"),
                        dim,
                    )));
                }
                Err(_) => {
                    push_error(app, "usage: /subagents [1-5]");
                }
            }
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("extensions") {
        if rest.is_empty() {
            // Same interface as /theme and /provider: a picker overlay
            // where enter toggles the selected extension.
            app.overlay = Some(Overlay::Extensions {
                picker: ListPicker::new(crate::extensions::EXTENSIONS.len()),
            });
            return;
        }
        if let Some(args) = rest.strip_prefix(' ') {
            let mut parts = args.split_whitespace();
            let enabled = match parts.next() {
                Some("enable" | "add" | "on") => true,
                Some("disable" | "remove" | "delete" | "off") => false,
                _ => {
                    push_error(app, "usage: /extensions [enable|disable <name>]");
                    return;
                }
            };
            let name = parts.next().unwrap_or("");
            if crate::extensions::find(name).is_none() {
                let known = crate::extensions::EXTENSIONS
                    .iter()
                    .map(|spec| spec.name)
                    .collect::<Vec<_>>()
                    .join(", ");
                push_error(
                    app,
                    format!("unknown extension: {name} — valid extensions: {known}"),
                );
                return;
            }
            let state = if enabled { "enabled" } else { "disabled" };
            match crate::config::save_extension(name, enabled) {
                Ok(_) => {
                    push_notice(
                        app,
                        format!("extension {name} {state} (applies to the next run)"),
                    );
                    if worker.send(WorkerCmd::ReloadExtensions).is_err() {
                        push_error(app, "worker is gone; restart orcacode");
                    }
                }
                Err(err) => {
                    push_error(
                        app,
                        format!("extension {name} not {state} (save failed: {err})"),
                    );
                }
            }
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("mcp") {
        if rest.is_empty() {
            // Same interface as /extensions: a picker overlay where
            // space toggles the selected server. With nothing
            // configured the overlay would be a dead end, so the hint
            // stands in for it.
            let servers = crate::config::stored_mcp_servers();
            if servers.is_empty() {
                push_notice(app, "no MCP servers configured — /mcp add <name> <command>");
                return;
            }
            app.overlay = Some(Overlay::Mcp {
                picker: ListPicker::new(servers.len()),
                filter: String::new(),
                servers,
            });
            return;
        }
        if let Some(args) = rest.strip_prefix(' ') {
            mcp_command(app, args, worker);
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("copy") {
        if rest.is_empty() || rest.starts_with(' ') {
            copy_command(app, rest.trim());
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("skills") {
        if rest.is_empty() || rest.starts_with(' ') {
            skills_command(app, rest.trim(), worker);
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("models") {
        if rest.is_empty() || rest.starts_with(' ') {
            // Fetch the full catalog; the argument seeds the picker's
            // live filter so the user can widen it without refetching.
            app.picker_pending = Some(rest.trim().to_lowercase());
            if worker
                .send(WorkerCmd::ListModels {
                    filter: String::new(),
                })
                .is_err()
            {
                app.picker_pending = None;
                push_error(app, "worker is gone; restart orcacode");
            } else {
                push_notice(app, "fetching models…");
            }
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("sessions") {
        let arg = rest.trim();
        let Some(base) = crate::config::sessions_dir() else {
            push_error(app, "no home directory for session storage");
            return;
        };
        let dir = base.join(orca_harness_extensions::workspace_key(
            &app.cfg.workspace_root,
        ));
        let sessions = orca_harness_extensions::SessionFile::list(&dir);
        if arg.is_empty() {
            if sessions.is_empty() {
                push_notice(app, "no recorded sessions for this workspace");
                return;
            }
            // Same interface as /provider and /theme: a picker overlay,
            // preselected on the current session. The display windows to
            // the last few sessions (newest first), and ↑↓/PgUp/PgDn
            // navigate the whole list just like the /models picker.
            let index = sessions
                .iter()
                .position(|s| app.cfg.session_id.as_deref() == Some(s.meta.id.as_str()))
                .unwrap_or(0)
                // An old-but-current session stays reachable; the window
                // just opens on the newest rows.
                .min(SESSIONS_WINDOW.saturating_sub(1));
            app.overlay = Some(Overlay::Sessions {
                picker: ListPicker::with_selected(sessions.len(), index).actions(SESSION_ACTIONS),
                sessions,
            });
            return;
        }
        match sessions.iter().find(|s| s.meta.id.starts_with(arg)) {
            Some(session) => {
                if worker
                    .send(WorkerCmd::LoadSession {
                        path: session.path.clone(),
                    })
                    .is_err()
                {
                    push_error(app, "worker is gone; restart orcacode");
                } else {
                    push_notice(app, format!("loading session {}…", session.meta.id));
                }
            }
            None => {
                push_error(
                    app,
                    format!("no session matching {arg} — /sessions lists them"),
                );
            }
        }
        return;
    }
    if let Some(rest) = command.strip_prefix("theme") {
        let arg = rest.trim();
        if arg.is_empty() {
            // Same interface as /models and /provider: a picker overlay,
            // preselected on the active theme.
            let current = view::theme_name();
            let index = view::ThemeName::ALL
                .iter()
                .position(|name| *name == current)
                .unwrap_or(0);
            app.overlay = Some(Overlay::Themes {
                picker: ListPicker::with_selected(view::ThemeName::ALL.len(), index),
            });
            return;
        }
        match view::ThemeName::from_str(arg) {
            Some(name) => {
                view::set_theme(name);
                let note = match crate::config::save_theme(name.slug()) {
                    Ok(_) => format!("theme set to {}", name.label()),
                    Err(err) => format!("theme set to {} (not saved: {err})", name.label()),
                };
                push_notice(app, note);
            }
            None => {
                push_error(app, format!(
                        "unknown theme: {arg} — valid themes: default, mono, dracula, solarized-dark, one-dark, monokai, nord"
                    ));
            }
        }
        return;
    }
    match command {
        "quit" | "exit" | "q" => app.quit = true,
        "clear" => {
            let _ = worker.send(WorkerCmd::Clear);
            reset_conversation_ui(app);
        }
        "usage" => {
            app.overlay = Some(Overlay::Usage);
        }
        "compact" => {
            if worker.send(WorkerCmd::Compact).is_err() {
                push_error(app, "worker is gone; restart orcacode");
            } else {
                push_notice(app, "compacting conversation…");
            }
        }
        "fork" => {
            if worker.send(WorkerCmd::Fork).is_err() {
                push_error(app, "worker is gone; restart orcacode");
            } else {
                push_notice(app, "forking session…");
            }
        }
        "provider" => {
            app.overlay = Some(Overlay::Providers {
                picker: ListPicker::new(Provider::ALL.len()),
            });
        }
        "settings" => {
            app.overlay = Some(Overlay::Settings {
                picker: ListPicker::new(SETTINGS_ROWS),
            });
        }
        "help" | "" => {
            for entry in [
                "/help        show this help",
                "/expand [n]  full output of the n-th latest tool call (1 = latest)",
                "/clear       reset the conversation, empty the session, stop background work",
                "/compact     compact the conversation (elide tool outputs, capped summary)",
                "/usage       session token totals, cache traffic, and context occupancy",
                "/copy [code|all|tool] copy the last answer, its last code block, the transcript, or the inspected tool",
                "/sessions [id] resume a recorded session (no argument opens the picker)",
                "/queue [clear] show or clear waiting prompts",
                "/models [f]  pick a model from the endpoint's catalog",
                "/provider    switch provider (openrouter, openai, local)",
                "/settings    view and change provider, model, theme, api key",
                "/subagents [n] show or set subagent nesting depth (1-5)",
                "/extensions  toggle harness extensions (no argument opens the picker)",
                "/mcp         toggle MCP servers · add <name> <command> · remove <name>",
                "/skills      list and toggle skills (space reveals toggle/delete)",
                "/skills add <source>   install from owner/repo, a url, or a local folder",
                "/skills create <name>  scaffold a new skill · remove <name> · reload",
                "/quit        exit",
                "/theme [name] pick a color theme (no argument opens the picker)",
                "@path        add a workspace file or folder to the prompt",
                "keys: enter send or queue · esc cancel run · ctrl+o reveal latest work tree",
                "      scroll: wheel · shift+↑/↓ line · pgup/pgdn page",
                "      ctrl+c quit · up/down history",
                "      ctrl+y copy last answer",
                "copying: ctrl+y and /copy use OSC 52 (works over ssh; tmux needs set-clipboard on).",
                "         the wheel scrolls; to drag-select, hold option (macOS) or shift.",
                "         a drag selects whole terminal rows, so in split view it takes both panes;",
                "         ctrl+y copies just the focused one (tab focuses the inspector).",
                "approvals: y allow once · a always (session) · A always (saved for this workspace) · n deny",
            ] {
                app.push_line(Line::from(Span::styled(entry.to_string(), dim)));
            }
        }
        other => {
            push_error(app, format!("unknown command: /{other}"));
        }
    }
}

/// `/mode [normal|plan]` — no argument toggles, which is what a mode
/// with two states wants. The change lands on the shared handle the plan
/// gate reads per tool call, so it takes effect on the call in flight
/// with no agent rebuild and nothing to save.
fn mode_command(app: &mut App, arg: &str) {
    use crate::mode::Mode;
    let next = if arg.is_empty() {
        app.cfg.mode.toggle()
    } else {
        match Mode::from_label(arg) {
            Some(mode) => {
                app.cfg.mode.set(mode);
                mode
            }
            None => {
                push_error(
                    app,
                    format!("unknown mode: {arg} — valid modes: normal, plan"),
                );
                return;
            }
        }
    };
    push_notice(
        app,
        format!("{} mode · {}", next.label(), next.description()),
    );
    // Leaving plan mode ends the episode. Report the plans the agent
    // actually wrote — observed from tool results, not guessed — and say
    // nothing when it wrote none: plan mode is also a fine way to just
    // look around, and announcing a missing file would be nagging.
    if next == Mode::Normal {
        for path in app.cfg.plan.end() {
            push_notice(app, format!("plan saved to {path}"));
        }
    }
}

/// `/rewind [n]` — drop the last n user turns (default 1) from the
/// conversation and from the recorded session, so the next prompt
/// continues from before them.
fn rewind_command(app: &mut App, arg: &str, worker: &mpsc::UnboundedSender<WorkerCmd>) {
    let turns = if arg.is_empty() {
        1
    } else {
        match arg.parse::<usize>() {
            Ok(0) | Err(_) => {
                push_error(
                    app,
                    "usage: /rewind [n] — n is how many turns to drop (default 1)",
                );
                return;
            }
            Ok(turns) => turns,
        }
    };
    if worker.send(WorkerCmd::Rewind { turns }).is_err() {
        push_error(app, "worker is gone; restart orcacode");
    }
}

/// `/todo` — the agent's current task list, as `todo_write` last left it.
fn todo_command(app: &mut App, width: usize) {
    use orca_harness_tools::TodoStatus;
    let items = app.cfg.todos.items();
    if items.is_empty() {
        push_notice(app, "no task list — the agent writes one with todo_write");
        return;
    }
    let t = theme();
    let (done, total) = app.cfg.todos.progress();
    let mut lines = vec![Line::from(vec![
        Span::styled("  todo", t.strong),
        Span::styled(format!(" · {done}/{total} done"), t.dim),
    ])];
    for item in items {
        let (marker, style) = match item.status {
            TodoStatus::Completed => ("✓", t.dim),
            TodoStatus::InProgress => ("▸", t.strong),
            TodoStatus::Pending => ("□", t.dim),
        };
        lines.push(Line::from(Span::styled(
            view::truncate_line(&format!("  {marker} {}", item.content), width),
            style,
        )));
    }
    app.push_transcript_block(lines, BlockSpacing::Tight);
}

/// `/skills [add <source> | create <name> | remove <name> | show <name>
/// | reload]`.
///
/// `add` takes what the `npx skills` ecosystem takes — `owner/repo`,
/// `owner/repo@skill`, a GitHub or skills.sh URL, a local folder, even a
/// pasted `npx skills add …` line — and installs beside `config.json`
/// unless `--here` puts it in the project. `create` scaffolds a new one
/// in the project. Cloning happens in the worker, so the interface stays
/// responsive.
fn skills_command(app: &mut App, args: &str, worker: &mpsc::UnboundedSender<WorkerCmd>) {
    let dim = theme().dim;
    let mut parts = args.split_whitespace();
    match parts.next() {
        None => {
            let entries = app.cfg.skills.catalog();
            if entries.is_empty() {
                for line in [
                    "no skills yet:",
                    "  /skills add <owner/repo>   install from a repository or folder",
                    "  /skills create <name>      scaffold one in .orca/skills",
                    "found automatically in .orca/skills, skills, .claude/skills,",
                    ".agents/skills, .codex/skills, .opencode/skills — and the same under ~",
                ] {
                    app.push_line(Line::from(Span::styled(line, dim)));
                }
                return;
            }
            app.overlay = Some(Overlay::Skills {
                picker: ListPicker::new(entries.len()).actions(SKILL_ACTIONS),
                filter: String::new(),
                entries,
            });
        }
        Some("add" | "install") => {
            let rest = args
                .split_once(char::is_whitespace)
                .map(|(_, rest)| rest.trim())
                .unwrap_or("");
            // `--here` is consumed here rather than in the parser: it is
            // about where this host puts things, not about the source.
            let here = rest.split_whitespace().any(|token| token == "--here");
            let source: String = rest
                .split_whitespace()
                .filter(|token| *token != "--here")
                .collect::<Vec<_>>()
                .join(" ");
            if source.is_empty() {
                for line in [
                    "usage: /skills add <source> [--skill <name>] [--list] [--here]",
                    "  source: owner/repo · owner/repo@skill · a github or skills.sh url ·",
                    "          a local folder · a pasted `npx skills add …` line",
                    "  --here installs into .orca/skills instead of your config folder",
                ] {
                    app.push_line(Line::from(Span::styled(line, dim)));
                }
                return;
            }
            let cmd = WorkerCmd::InstallSkill {
                source: source.clone(),
                here,
            };
            if worker.send(cmd).is_err() {
                push_error(app, "worker is gone; restart orcacode");
            } else {
                push_notice(app, format!("fetching {source}…"));
            }
        }
        Some("create" | "new") => {
            let Some(name) = parts.next() else {
                push_error(app, "usage: /skills create <name> [--global]");
                return;
            };
            let global = parts.any(|token| token == "--global" || token == "-g");
            match app.cfg.skills.create(name, global) {
                Ok(path) => {
                    push_notice(
                        app,
                        format!("created {} — edit it, then /skills reload", path.display()),
                    );
                    let _ = worker.send(WorkerCmd::ReloadSkills);
                }
                Err(err) => {
                    push_error(app, format!("skill not created: {err}"));
                }
            }
        }
        Some("remove" | "delete" | "rm" | "uninstall") => {
            let Some(name) = parts.next() else {
                push_error(app, "usage: /skills remove <name>");
                return;
            };
            remove_skill(app, name, worker);
        }
        Some("reload") => {
            // The rescan itself is the worker's, so the tool the agent
            // carries and the catalog on screen never disagree.
            if worker.send(WorkerCmd::ReloadSkills).is_err() {
                push_error(app, "worker is gone; restart orcacode");
            } else {
                push_notice(app, "rescanning skills…");
            }
        }
        Some("show") => {
            let Some(name) = parts.next() else {
                push_error(app, "usage: /skills show <name>");
                return;
            };
            let entries = app.cfg.skills.catalog();
            let Some(entry) = entries.iter().find(|entry| entry.name == name) else {
                let known = match entries.len() {
                    0 => "none found".to_string(),
                    _ => entries
                        .iter()
                        .map(|entry| entry.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                };
                push_error(app, format!("unknown skill: {name} — found: {known}"));
                return;
            };
            let detail = match &entry.state {
                crate::skills::SkillState::Loaded { root, bytes } => format!(
                    "{name} · {root} · {} · {}",
                    size(*bytes),
                    if entry.enabled { "on" } else { "off" }
                ),
                crate::skills::SkillState::Shadowed { root, by } => {
                    format!("{name} · {root} · shadowed by the copy in {by}")
                }
                crate::skills::SkillState::Failed { root, reason } => {
                    format!("{name} · {root} · failed — {reason}")
                }
            };
            app.push_line(Line::from(Span::styled(detail, dim)));
            if !entry.description.is_empty() {
                app.push_line(Line::from(Span::styled(
                    format!("  {}", entry.description),
                    dim,
                )));
            }
        }
        Some(other) => {
            push_error(
                app,
                format!(
                    "unknown /skills argument: {other} — usage: /skills \
                     [add <source> | create <name> | remove <name> | show <name> | reload]"
                ),
            );
        }
    }
}

/// Delete an installed skill and rescan. Shared by the typed form and
/// the overlay's action strip, so both refuse the same things: only what
/// this host installed is deletable.
fn remove_skill(app: &mut App, name: &str, worker: &mpsc::UnboundedSender<WorkerCmd>) -> bool {
    match app.cfg.skills.remove(name) {
        Ok(note) => {
            push_notice(app, note);
            let _ = worker.send(WorkerCmd::ReloadSkills);
            true
        }
        Err(err) => {
            push_notice(app, err);
            false
        }
    }
}

/// The typed `/mcp add|remove` forms: edit the config, then ask the
/// worker to reconnect and rebuild — the same save-then-reload shape as
/// the typed /extensions form.
fn mcp_command(app: &mut App, args: &str, worker: &mpsc::UnboundedSender<WorkerCmd>) {
    let usage = "usage: /mcp [add <name> <command> | remove <name>]";
    let mut parts = args.split_whitespace();
    match parts.next() {
        Some("add") => {
            let name = parts.next().unwrap_or("");
            let launch = parts.collect::<Vec<_>>().join(" ");
            if name.is_empty() || launch.is_empty() {
                push_error(app, usage);
                return;
            }
            // The name becomes part of the model-facing tool names
            // (mcp__<name>__<tool>), so keep it identifier-shaped.
            if !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                push_error(
                    app,
                    format!("invalid server name: {name} — letters, digits, - and _ only"),
                );
                return;
            }
            // Editing a server that the user turned off must not
            // quietly turn it back on, so say what actually happens
            // rather than promising a connection that will not run.
            let disabled = crate::config::stored_mcp_servers()
                .iter()
                .any(|server| server.name == name && !server.enabled);
            match crate::config::save_mcp_server(name, &launch) {
                Ok(_) => {
                    let note = if disabled {
                        format!("mcp server {name} updated — still off, space in /mcp enables it")
                    } else {
                        format!("mcp server {name} added — connecting…")
                    };
                    push_notice(app, note);
                    if worker.send(WorkerCmd::ReloadMcp).is_err() {
                        push_error(app, "worker is gone; restart orcacode");
                    }
                }
                Err(err) => {
                    push_error(
                        app,
                        format!("mcp server {name} not added (save failed: {err})"),
                    );
                }
            }
        }
        Some("remove" | "delete" | "rm") => {
            let name = parts.next().unwrap_or("");
            let servers = crate::config::stored_mcp_servers();
            if !servers.iter().any(|server| server.name == name) {
                let known = match servers.len() {
                    0 => "none configured".to_string(),
                    _ => servers
                        .iter()
                        .map(|server| server.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                };
                push_error(
                    app,
                    format!("unknown mcp server: {name} — configured: {known}"),
                );
                return;
            }
            match crate::config::remove_mcp_server(name) {
                Ok(_) => {
                    push_notice(
                        app,
                        format!("mcp server {name} removed (applies to the next run)"),
                    );
                    if worker.send(WorkerCmd::ReloadMcp).is_err() {
                        push_error(app, "worker is gone; restart orcacode");
                    }
                }
                Err(err) => {
                    push_error(
                        app,
                        format!("mcp server {name} not removed (save failed: {err})"),
                    );
                }
            }
        }
        _ => {
            push_error(app, usage);
        }
    }
}

fn handle_ui_msg(
    app: &mut App,
    msg: UiMsg,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    width: usize,
) {
    match msg {
        UiMsg::Event(event) => handle_harness_event(app, event, width),
        UiMsg::SubagentEvent {
            id,
            parent_id,
            depth,
            call_id,
            event,
        } => handle_subagent_event(app, id, parent_id, depth, call_id, event),
        UiMsg::Approval(request) => app.approval = Some(request),
        UiMsg::Models(result) => {
            let t = theme();
            let seed = app.picker_pending.take().unwrap_or_default();
            // Learn the active model's window from the catalog in passing.
            if let Ok(models) = &result {
                if let Some(info) = models.iter().find(|m| m.id == app.cfg.model_name) {
                    if info.context_length.is_some() {
                        app.context_window = info.context_length;
                    }
                }
            }
            match result {
                Ok(models) if models.is_empty() => {
                    app.push_line(Line::from(Span::styled("no models available", t.dim)));
                }
                Ok(models) => {
                    app.overlay = Some(Overlay::Models(ModelPicker {
                        models,
                        filter: seed,
                        index: 0,
                    }));
                }
                Err(err) => app.push_line(Line::from(Span::styled(
                    format!("model list failed: {err}"),
                    t.error,
                ))),
            }
        }
        UiMsg::ModelChanged(id) => {
            app.cfg.model_name = id.clone();
            app.push_line(Line::from(Span::styled(
                format!("model: {id}"),
                theme().dim,
            )));
        }
        UiMsg::ContextWindow(window) => {
            // Best-effort discovery: never wipe a window the model picker
            // already stashed with a probe that found nothing.
            if window.is_some() {
                app.context_window = window;
            }
        }
        UiMsg::Compacted(result) => match result {
            Ok(report) => {
                // Until the next model step reports real usage, the
                // report's estimate is the best context figure we have.
                app.context_tokens = report.est_tokens_after as u64;
                let pct =
                    100.0 * report.est_tokens_after as f64 / report.est_tokens_before.max(1) as f64;
                app.push_line(Line::from(Span::styled(
                    format!(
                        "compacted: {} -> {} messages · ~{} -> ~{} est tokens ({pct:.1}%)",
                        report.messages_before,
                        report.messages_after,
                        report.est_tokens_before,
                        report.est_tokens_after,
                    ),
                    theme().dim,
                )));
                if report.elided_results > 0 {
                    app.push_line(Line::from(Span::styled(
                        format!(
                            "{} tool outputs ({} KB) elided to store, recoverable via read_tool_result",
                            report.elided_results,
                            report.elided_bytes / 1024,
                        ),
                        theme().dim,
                    )));
                }
            }
            Err(err) => {
                app.push_line(Line::from(Span::styled(
                    format!("compact: {err}"),
                    theme().dim,
                )));
            }
        },
        UiMsg::Notice(text) => {
            push_notice(app, text);
        }
        UiMsg::SessionCleared { id } => {
            app.cfg.session_id = Some(id.clone());
            push_notice(
                app,
                format!("session {id} cleared · background work stopped"),
            );
        }
        UiMsg::ContextRewound { messages, notice } => {
            // The transcript is redrawn from the shortened context, but
            // the token totals are not conversation state — they record
            // what this session actually spent, and rewinding does not
            // un-spend it. Occupancy is left to the next model step.
            let spent = (
                app.tokens_in,
                app.tokens_out,
                app.cache_read_total,
                app.cache_write_total,
                app.usage_steps,
            );
            reset_conversation_ui(app);
            (
                app.tokens_in,
                app.tokens_out,
                app.cache_read_total,
                app.cache_write_total,
                app.usage_steps,
            ) = spent;
            push_notice(app, notice);
            replay_transcript(app, &messages, width);
        }
        UiMsg::SessionForked { id, parent } => {
            app.cfg.session_id = Some(id.clone());
            push_notice(
                app,
                format!("forked to session {id} · {parent} is left as it was"),
            );
        }
        UiMsg::SessionLoaded { id, messages } => {
            reset_conversation_ui(app);
            app.cfg.session_id = Some(id.clone());
            push_notice(
                app,
                format!("resumed session {id} ({} messages)", messages.len()),
            );
            replay_transcript(app, &messages, width);
        }
        UiMsg::ProviderChanged { provider, model } => {
            app.cfg.provider = provider;
            app.cfg.model_name = model.clone();
            app.context_window = None;
            app.push_line(Line::from(Span::styled(
                format!("provider: {} · model: {model}", provider.label()),
                theme().dim,
            )));
        }
        UiMsg::RunDone(result) => {
            let completed = result.is_ok();
            let turn_elapsed = match &app.run {
                RunState::Running { started, .. } => Some(started.elapsed()),
                RunState::Idle => None,
            };
            // Flush any partial stream (interrupted mid-generation).
            if result.is_err() {
                app.commit_activity(width);
                let partial = if !app.text.trim().is_empty() {
                    Some(std::mem::take(&mut app.text))
                } else {
                    app.pending_assistant.take()
                };
                if let Some(partial) = partial {
                    app.push_markdown_block(&partial, width, BlockSpacing::Section);
                    // Interrupted output is often exactly what the user
                    // wanted to keep — that is why they interrupted.
                    app.last_answer = Some(partial);
                }
            }
            app.text.clear();
            app.run = RunState::Idle;
            app.approval = None;
            if let Err(err) = result {
                let (style, label) = if err.to_lowercase().contains("cancel") {
                    (theme().dim, "interrupted".to_string())
                } else {
                    (theme().error, format!("run failed: {err}"))
                };
                let mut lines = Vec::new();
                push_wrapped_lines(&mut lines, &label, "  ", style, width);
                app.push_transcript_block(lines, BlockSpacing::Tight);
            }
            if completed {
                if let Some(elapsed) = turn_elapsed {
                    let calls = app.turn_tool_calls;
                    let plural = if calls == 1 { "" } else { "s" };
                    app.last_turn_summary = Some(format!(
                        "Turn took {:.1}s and took {calls} tool call{plural}",
                        elapsed.as_secs_f64(),
                    ));
                }
                start_next_queued_prompt(app, worker, width);
            }
        }
        UiMsg::ShellDone => {
            app.commit_activity(width);
            let elapsed = match &app.run {
                RunState::Running { started, .. } => Some(started.elapsed()),
                RunState::Idle => None,
            };
            app.run = RunState::Idle;
            app.approval = None;
            if let Some(elapsed) = elapsed {
                app.last_turn_summary =
                    Some(format!("Shell command took {:.1}s", elapsed.as_secs_f64()));
            }
            start_next_queued_prompt(app, worker, width);
        }
    }
}

fn handle_harness_event(app: &mut App, event: HarnessEvent, width: usize) {
    match event {
        HarnessEvent::AssistantDelta { text } => {
            if app.text.is_empty() && !text.is_empty() {
                app.commit_settled_tools(width);
            }
            app.text.push_str(&text);
        }
        HarnessEvent::ReasoningDelta { text } => {
            if app.reasoning.is_empty() && !text.is_empty() {
                app.commit_settled_tools(width);
                app.reasoning_started = Some(Instant::now());
            }
            app.reasoning.push_str(&text);
        }
        HarnessEvent::Assistant { message } => {
            // `Assistant` closes the current model phase. Commit its
            // reasoning and any preceding tool batch before retaining the
            // message that follows it in the event stream.
            app.commit_activity(width);
            app.text.clear();
            app.pending_assistant = Some(message);
        }
        HarnessEvent::ToolCall {
            tool_call_id,
            tool_name,
            input,
        } => {
            clear_tool_connectors(&mut app.transcript);
            clear_tool_connectors(&mut app.pending_history);
            app.flush_reasoning();
            if let Some(message) = app.pending_assistant.take() {
                if !message.trim().is_empty() {
                    app.push_markdown_block(&message, width, BlockSpacing::Tight);
                    // Prose said on the way to a tool call is still the
                    // most recent thing the model wrote.
                    app.last_answer = Some(message);
                }
            }
            let call_line = view::tool_call_line(&tool_name, &input);
            app.turn_tool_calls += 1;
            let index = app.activity_tools.len();
            app.activity_tools.push(ToolActivity {
                call_id: tool_call_id.clone(),
                call_line,
                tool_name,
                input,
                started: Instant::now(),
                elapsed: None,
                output: None,
                is_error: false,
                approval: None,
            });
            if !app.split_focused {
                app.split_tool = Some(index);
                app.split_scroll = 0;
            }
            app.pending_calls.insert(tool_call_id, index);
        }
        HarnessEvent::ToolResult {
            tool_call_id,
            tool_name,
            output,
            is_error,
        } => {
            // Trailing estimate, pi-style: the result joins the context
            // now but is only billed at the next model step, which then
            // overwrites this with the provider's count.
            let result_bytes = serde_json::to_string(&output).map(|s| s.len()).unwrap_or(0);
            app.context_tokens += (result_bytes / 4) as u64;
            let index = app.pending_calls.remove(&tool_call_id);
            let call_line = index
                .and_then(|index| app.activity_tools.get_mut(index))
                .map(|activity| {
                    activity.elapsed = Some(activity.started.elapsed());
                    activity.output = Some(output.clone());
                    activity.is_error = is_error;
                    activity.call_line.clone()
                })
                .unwrap_or_else(|| tool_name.clone());
            let inner = if tool_name == "subagent" {
                fold_subagent_activity(app, &tool_call_id)
            } else {
                Vec::new()
            };
            app.push_record(ToolRecord {
                call_line,
                tool_name,
                output,
                inner,
            });
        }
        HarnessEvent::Usage { usage } => {
            app.tokens_in += usage.input_tokens;
            app.tokens_out += usage.output_tokens;
            app.cache_read_total += usage.cache_read_tokens;
            app.cache_write_total += usage.cache_create_tokens;
            app.usage_steps += 1;
            // The latest step's full footprint (uncached + cached input +
            // output) is what the next request will carry. Authoritative:
            // replaces any bytes/4 estimates accumulated since last step.
            app.context_tokens = usage.context_tokens();
        }
        HarnessEvent::Result { message } => {
            app.commit_activity(width);
            let answer = if message.is_empty() {
                app.pending_assistant.take().unwrap_or_default()
            } else {
                app.pending_assistant = None;
                message
            };
            app.text.clear();
            if !answer.trim().is_empty() {
                app.push_markdown_block(&answer, width, BlockSpacing::Section);
                app.last_answer = Some(answer);
            }
        }
        HarnessEvent::AgentStart | HarnessEvent::Error { .. } => {}
    }
}

/// Remove all spawn activity anchored to a finished subagent call
/// (including nested descendants) and render it to plain lines for the
/// expandable record.
fn fold_subagent_activity(app: &mut App, call_id: &str) -> Vec<String> {
    let mut roots: Vec<u64> = app
        .subagent_activity
        .iter()
        .filter(|(_, spawn)| spawn.call_id == call_id)
        .map(|(id, _)| *id)
        .collect();
    roots.sort_unstable();
    let mut lines = Vec::new();
    for id in roots {
        collect_spawn_log(app, id, &mut lines);
    }
    lines
}

fn collect_spawn_log(app: &mut App, id: u64, lines: &mut Vec<String>) {
    let Some(spawn) = app.subagent_activity.remove(&id) else {
        return;
    };
    let indent = "  ".repeat(spawn.depth as usize);
    for tool in &spawn.tools {
        let glyph = match &tool.output {
            Some(_) if tool.is_error => "×",
            Some(_) => "✓",
            None => "□",
        };
        let elapsed = tool.elapsed.unwrap_or_default();
        lines.push(format!(
            "{indent}{glyph} {} · {}",
            tool.call_line,
            elapsed_label(elapsed)
        ));
    }
    let mut children: Vec<u64> = app
        .subagent_activity
        .iter()
        .filter(|(_, s)| s.parent_id == Some(id))
        .map(|(child, _)| *child)
        .collect();
    children.sort_unstable();
    for child in children {
        collect_spawn_log(app, child, lines);
    }
}

/// Inner subagent lifecycle: only tool calls/results feed the nested
/// rail; inner deltas and text stay hidden by design.
fn handle_subagent_event(
    app: &mut App,
    id: u64,
    parent_id: Option<u64>,
    depth: u32,
    call_id: String,
    event: HarnessEvent,
) {
    match event {
        HarnessEvent::ToolCall {
            tool_call_id,
            tool_name,
            input,
        } => {
            let spawn = app
                .subagent_activity
                .entry(id)
                .or_insert_with(|| SpawnActivity {
                    call_id,
                    parent_id,
                    depth,
                    tools: Vec::new(),
                    pending: std::collections::HashMap::new(),
                });
            let call_line = view::tool_call_line(&tool_name, &input);
            let index = spawn.tools.len();
            spawn.tools.push(ToolActivity {
                call_id: tool_call_id.clone(),
                call_line,
                tool_name,
                input,
                started: Instant::now(),
                elapsed: None,
                output: None,
                is_error: false,
                approval: None,
            });
            spawn.pending.insert(tool_call_id, index);
        }
        HarnessEvent::ToolResult {
            tool_call_id,
            output,
            is_error,
            ..
        } => {
            if let Some(spawn) = app.subagent_activity.get_mut(&id) {
                if let Some(index) = spawn.pending.remove(&tool_call_id) {
                    if let Some(tool) = spawn.tools.get_mut(index) {
                        tool.elapsed = Some(tool.started.elapsed());
                        tool.output = Some(output);
                        tool.is_error = is_error;
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::backend::TestBackend;

    fn paste(app: &mut App, tx: &mpsc::UnboundedSender<WorkerCmd>, text: &str) {
        handle_terminal_event(app, CtEvent::Paste(text.to_string()), tx, 80);
    }

    /// The bug this replaced: without bracketed paste every newline
    /// arrived as enter, so a pasted block submitted its first line and
    /// queued the rest.
    #[test]
    fn a_multiline_paste_is_one_marker_and_submits_nothing() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        let block = (1..=23)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");

        paste(&mut app, &tx, &block);

        assert_eq!(app.composer, "[Pasted text #1, 23 lines]");
        assert_eq!(app.cursor, app.composer.chars().count());
        assert!(app.prompt_queue.is_empty(), "paste must not queue prompts");
        assert!(rx.try_recv().is_err(), "paste must not start a run");
    }

    #[test]
    fn a_marker_expands_to_the_held_text_on_send() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();

        paste(&mut app, &tx, "alpha\nbeta\ngamma");
        for c in " explain this".chars() {
            press(&mut app, &tx, KeyCode::Char(c));
        }
        assert_eq!(app.composer, "[Pasted text #1, 3 lines] explain this");
        submit(&mut app, &tx, 80);

        match rx.try_recv() {
            Ok(WorkerCmd::Run { prompt, .. }) => {
                assert_eq!(prompt, "alpha\nbeta\ngamma explain this");
            }
            other => panic!("expected a run, got {:?}", other.is_ok()),
        }
        // Once sent, the turn shows what the model got, not the marker.
        let shown = app
            .pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            shown.contains("alpha") && shown.contains("beta") && shown.contains("gamma"),
            "transcript should unfurl the paste: {shown}"
        );
        assert!(
            !shown.contains("[Pasted text #"),
            "no marker should survive into the transcript: {shown}"
        );
    }

    #[test]
    fn backspace_removes_a_marker_whole() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        for c in "look at ".chars() {
            press(&mut app, &tx, KeyCode::Char(c));
        }
        paste(&mut app, &tx, "alpha\nbeta\ngamma");

        press(&mut app, &tx, KeyCode::Backspace);

        assert_eq!(app.composer, "look at ");
        assert_eq!(app.cursor, 8);
    }

    #[test]
    fn delete_removes_a_marker_whole_from_its_start() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        paste(&mut app, &tx, "alpha\nbeta");
        for c in " done".chars() {
            press(&mut app, &tx, KeyCode::Char(c));
        }
        app.cursor = 0;

        press(&mut app, &tx, KeyCode::Delete);

        assert_eq!(app.composer, " done");
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn arrows_step_over_a_marker_rather_than_into_it() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        paste(&mut app, &tx, "alpha\nbeta");
        let width = app.composer.chars().count();

        press(&mut app, &tx, KeyCode::Left);
        assert_eq!(app.cursor, 0, "left clears the whole marker");

        press(&mut app, &tx, KeyCode::Right);
        assert_eq!(app.cursor, width, "right clears the whole marker");
    }

    /// Backspacing the chip must not strand the next paste's numbering.
    #[test]
    fn a_removed_marker_leaves_later_markers_expanding() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        paste(&mut app, &tx, "first\nblock");
        press(&mut app, &tx, KeyCode::Backspace);
        assert_eq!(app.composer, "");

        paste(&mut app, &tx, "second\nblock");
        assert_eq!(app.composer, "[Pasted text #2, 2 lines]");
        submit(&mut app, &tx, 80);

        match rx.try_recv() {
            Ok(WorkerCmd::Run { prompt, .. }) => assert_eq!(prompt, "second\nblock"),
            other => panic!("expected a run, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn a_short_single_line_paste_is_typed_through() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        for c in "run ".chars() {
            press(&mut app, &tx, KeyCode::Char(c));
        }

        paste(&mut app, &tx, "cargo test --all");

        assert_eq!(app.composer, "run cargo test --all");
        assert!(app.pastes.is_empty(), "nothing to hold aside");
    }

    #[test]
    fn crlf_pastes_are_normalized_before_counting() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();

        paste(&mut app, &tx, "one\r\ntwo\r\nthree\r\n");

        assert_eq!(app.composer, "[Pasted text #1, 3 lines]");
        assert_eq!(app.pastes[0], "one\ntwo\nthree\n");
    }

    #[test]
    fn markers_number_upward_and_expand_independently() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();

        paste(&mut app, &tx, "first\nblock");
        paste(&mut app, &tx, "second\nblock");
        assert_eq!(
            app.composer,
            "[Pasted text #1, 2 lines][Pasted text #2, 2 lines]"
        );

        submit(&mut app, &tx, 80);
        match rx.try_recv() {
            Ok(WorkerCmd::Run { prompt, .. }) => {
                assert_eq!(prompt, "first\nblocksecond\nblock");
            }
            other => panic!("expected a run, got {:?}", other.is_ok()),
        }
    }

    /// Text that merely looks like a marker is not a marker.
    #[test]
    fn typed_marker_lookalikes_are_left_alone() {
        assert_eq!(
            expand_pastes(&[], "[Pasted text #1, 3 lines]"),
            "[Pasted text #1, 3 lines]"
        );
        assert_eq!(
            expand_pastes(&["a\nb".to_string()], "[Pasted text #1, 9 lines]"),
            "[Pasted text #1, 9 lines]"
        );
    }

    fn test_app() -> App {
        let mut app = App::new(TuiConfig {
            model_name: "test".into(),
            workspace_name: "/workspace".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        });
        // A transcript taller than any viewport so scrolling has room.
        for i in 0..100 {
            app.transcript.push(Line::from(format!("line {i}")));
        }
        app
    }

    fn mouse(kind: MouseEventKind) -> CtEvent {
        mouse_at(kind, 0)
    }

    fn mouse_at(kind: MouseEventKind, column: u16) -> CtEvent {
        CtEvent::Mouse(MouseEvent {
            kind,
            column,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn mouse_wheel_scrolls_the_transcript_not_history() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.prompt_history.push("previous prompt".into());

        handle_terminal_event(&mut app, mouse(MouseEventKind::ScrollUp), &tx, 80);
        assert!(app.scroll > 0, "wheel up scrolls back");
        assert!(app.composer.is_empty(), "composer untouched by wheel");
        assert_eq!(app.history_pos, None, "history untouched by wheel");

        let scrolled = app.scroll;
        handle_terminal_event(&mut app, mouse(MouseEventKind::ScrollDown), &tx, 80);
        assert!(app.scroll < scrolled, "wheel down scrolls forward");
    }

    fn ctrl(code: char) -> CtEvent {
        CtEvent::Key(KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL))
    }

    /// The last notice or error the app pushed.
    fn last_notice(app: &App) -> String {
        app.pending_history
            .last()
            .map(line_text)
            .unwrap_or_default()
    }

    /// Copy takes the markdown the model actually produced, not the
    /// wrapped and highlighted lines the transcript holds — pasting the
    /// rendered form would carry the transcript's own indentation.
    #[test]
    fn ctrl_y_copies_the_last_answer_verbatim() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.last_answer = Some("# Title\n\nsome **prose**".into());

        handle_terminal_event(&mut app, ctrl('y'), &tx, 80);
        assert_eq!(
            app.clipboard_pending.as_deref(),
            Some("# Title\n\nsome **prose**")
        );
        assert!(last_notice(&app).contains("copied last answer"));
    }

    /// Hitting copy while the model is still typing should yield what is
    /// on screen, not the previous turn's answer under a notice claiming
    /// otherwise — a wrong clipboard is only discovered on paste.
    #[test]
    fn copy_mid_stream_takes_the_partial_answer_and_names_it() {
        let mut app = test_app();
        app.last_answer = Some("the previous turn".into());
        app.text = "half an answ".into();

        copy_command(&mut app, "");
        assert_eq!(app.clipboard_pending.as_deref(), Some("half an answ"));
        assert!(last_notice(&app).contains("answer so far"));

        // Once the stream closes the buffer empties and the answer stands.
        app.text.clear();
        app.clipboard_pending = None;
        copy_command(&mut app, "");
        assert_eq!(app.clipboard_pending.as_deref(), Some("the previous turn"));
        assert!(last_notice(&app).contains("copied last answer"));
    }

    #[test]
    fn copy_code_takes_the_last_fenced_block() {
        let mut app = test_app();
        app.last_answer = Some("try:\n```sh\ncargo test\n```\nthen ship".into());

        copy_command(&mut app, "code");
        assert_eq!(app.clipboard_pending.as_deref(), Some("cargo test"));
    }

    #[test]
    fn copy_all_flattens_the_transcript_to_plain_text() {
        let mut app = test_app();
        app.transcript.clear();
        app.transcript.push(Line::from(vec![
            Span::styled("• ", Style::default()),
            Span::styled("hello", Style::default()),
        ]));
        // Block spacing leaves trailing blanks that nobody wants pasted.
        app.transcript.push(Line::from(""));
        app.transcript.push(Line::from(""));

        copy_command(&mut app, "all");
        assert_eq!(app.clipboard_pending.as_deref(), Some("• hello"));
    }

    /// A copy that quietly does nothing is worse than one that refuses:
    /// the user pastes whatever was in the clipboard before and does not
    /// notice until it matters.
    #[test]
    fn copy_says_so_when_there_is_nothing_to_copy() {
        let mut app = test_app();
        app.last_answer = None;

        copy_command(&mut app, "");
        assert!(app.clipboard_pending.is_none());
        assert!(last_notice(&app).contains("nothing to copy"));

        app.last_answer = Some("prose with no code in it".into());
        copy_command(&mut app, "code");
        assert!(app.clipboard_pending.is_none());
        assert!(last_notice(&app).contains("last code block"));
    }

    /// Terminals truncate oversized OSC 52 payloads, and a half-copied
    /// answer pastes without any sign that it was cut.
    #[test]
    fn copy_refuses_a_payload_the_terminal_would_truncate() {
        let mut app = test_app();
        app.last_answer = Some("x".repeat(clipboard::MAX_COPY_BYTES + 1));

        copy_command(&mut app, "");
        assert!(app.clipboard_pending.is_none());
        assert!(last_notice(&app).contains("clipboard write"));
    }

    #[test]
    fn copy_rejects_an_unknown_target() {
        let mut app = test_app();
        app.last_answer = Some("something".into());

        copy_command(&mut app, "everything");
        assert!(app.clipboard_pending.is_none());
        assert!(last_notice(&app).contains("unknown /copy target"));
    }

    #[test]
    fn clearing_the_conversation_drops_the_copyable_answer() {
        let mut app = test_app();
        app.last_answer = Some("gone after /clear".into());

        reset_conversation_ui(&mut app);
        assert!(app.last_answer.is_none());
    }

    /// The palette shows a window onto the command list, so navigating
    /// past the last visible row has to slide it. Arrow keys, page keys,
    /// and the wheel all drive the same selection; before this, only the
    /// arrows did, and the wheel and page keys scrolled the transcript
    /// behind the open palette instead.
    #[test]
    fn palette_scrolls_its_own_list_by_arrows_page_keys_and_wheel() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.composer = "/".into();
        app.cursor = 1;
        let total = filter_commands("").len();
        assert!(
            total > PALETTE_ROWS,
            "this test needs more commands than fit: {total}"
        );

        // The window starts at the top and stays there while the
        // selection is inside it.
        let window = |app: &App| flat_lines(&palette_lines(app, PALETTE_ROWS + 2, 100));
        assert!(window(&app).contains(&format!("1-{PALETTE_ROWS}")));

        // One step past the last visible row slides the window by one.
        for _ in 0..PALETTE_ROWS {
            handle_terminal_event(
                &mut app,
                CtEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
                &tx,
                80,
            );
        }
        assert_eq!(app.palette_index, PALETTE_ROWS);
        assert!(
            window(&app).contains(&format!("2-{}", PALETTE_ROWS + 1)),
            "{}",
            window(&app)
        );
        assert_eq!(app.scroll, 0, "the transcript stays put");

        // Page keys move a screenful of the list, not of the transcript.
        handle_terminal_event(
            &mut app,
            CtEvent::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
            &tx,
            80,
        );
        assert_eq!(app.palette_index, 0);
        assert_eq!(app.scroll, 0, "page keys do not reach the transcript");
        handle_terminal_event(
            &mut app,
            CtEvent::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
            &tx,
            80,
        );
        assert_eq!(app.palette_index, PALETTE_ROWS);

        // The wheel does the same, one row at a time, and is clamped.
        handle_terminal_event(&mut app, mouse(MouseEventKind::ScrollUp), &tx, 80);
        assert_eq!(app.palette_index, PALETTE_ROWS - 1);
        assert_eq!(app.scroll, 0, "the wheel does not reach the transcript");
        for _ in 0..total * 2 {
            handle_terminal_event(&mut app, mouse(MouseEventKind::ScrollDown), &tx, 80);
        }
        assert_eq!(app.palette_index, total - 1, "clamped at the last entry");
        for _ in 0..total * 2 {
            handle_terminal_event(&mut app, mouse(MouseEventKind::ScrollUp), &tx, 80);
        }
        assert_eq!(app.palette_index, 0, "clamped at the first entry");
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn mouse_wheel_over_split_inspector_scrolls_only_the_inspector() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.view_mode = ViewMode::Split;
        let transcript_scroll = app.scroll;

        handle_terminal_event(
            &mut app,
            mouse_at(MouseEventKind::ScrollDown, 100),
            &tx,
            120,
        );
        assert_eq!(app.split_scroll, 3);
        assert_eq!(app.scroll, transcript_scroll, "left transcript stays put");

        handle_terminal_event(&mut app, mouse_at(MouseEventKind::ScrollUp, 100), &tx, 120);
        assert_eq!(app.split_scroll, 0);
    }

    #[test]
    fn scrolled_transcript_keeps_the_same_top_row_as_live_height_changes() {
        let mut app = test_app();
        app.transcript_max_scroll = 100;
        app.scroll = 10;
        let original_top = app.transcript_max_scroll - app.scroll;

        stabilize_transcript_scroll(&mut app, 112);
        assert_eq!(app.scroll, 22);
        assert_eq!(112 - app.scroll, original_top);

        stabilize_transcript_scroll(&mut app, 96);
        assert_eq!(app.scroll, 6);
        assert_eq!(96 - app.scroll, original_top);

        app.scroll = 0;
        stabilize_transcript_scroll(&mut app, 140);
        assert_eq!(app.scroll, 0, "bottom-follow mode remains at the bottom");
    }

    #[test]
    fn split_renders_new_transcript_blocks_at_the_left_panes_real_width() {
        let mut app = test_app();
        app.transcript.clear();
        app.view_mode = ViewMode::Split;
        let width = transcript_content_width(&app, 120);
        assert_eq!(width, 69);

        app.push_markdown_block(
            "| Component | Responsibility | Notes |\n|---|---|---|\n| harness-core | Dispatch and concurrency | deterministic ordered results |",
            width,
            BlockSpacing::Section,
        );
        assert!(
            app.pending_history
                .iter()
                .all(|line| line_text(line).chars().count() <= width),
            "markdown is laid out for the pane before it is committed"
        );
    }

    #[test]
    fn inspector_caps_large_file_previews() {
        let output = serde_json::Value::String(
            (0..400)
                .map(|line| format!("line {line}: {}", "x".repeat(200)))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let (preview, omitted) = inspector_output_preview("read_file", &output, None, false);
        assert!(omitted);
        assert!(preview.len() <= INSPECTOR_PREVIEW_CHARS);
        assert!(preview.lines().count() <= INSPECTOR_PREVIEW_LINES);
    }

    #[test]
    fn inspector_shell_output_leads_with_signal_and_keeps_the_tail() {
        let stdout = (0..40)
            .map(|line| format!("build line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let output = serde_json::json!({
            "stdout": stdout,
            "stderr": "",
            "exitCode": 0,
            "success": true
        });
        let (preview, omitted) = inspector_output_preview("shell", &output, None, false);
        assert!(preview.starts_with("exit 0 · 40 stdout\n"));
        assert!(preview.contains("build line 0"));
        assert!(preview.contains("build line 39"));
        assert!(!preview.contains("build line 20"));
        assert!(omitted);
    }

    #[test]
    fn inspector_collection_output_shows_count_without_json_scaffolding() {
        let output = serde_json::json!({
            "query": "ToolActivity",
            "matches": ["src/tui.rs:181", "src/tui.rs:1861"],
            "truncated": false
        });
        let (preview, omitted) = inspector_output_preview("grep", &output, Some("json"), false);
        assert_eq!(preview, "2 matches\nsrc/tui.rs:181\nsrc/tui.rs:1861");
        assert!(!omitted);
    }

    #[test]
    fn inspector_classifies_results_by_shape_not_tool_name() {
        let execution = serde_json::json!({
            "stdout": "compiled",
            "stderr": "",
            "exitCode": 0
        });
        let (preview, _) = inspector_output_preview("custom_runner", &execution, None, false);
        assert_eq!(preview, "exit 0 · 1 stdout\ncompiled");

        let mutation = serde_json::json!({"path": "src/lib.rs", "bytesWritten": 2048});
        let (preview, _) = inspector_output_preview("custom_writer", &mutation, None, false);
        assert_eq!(preview, "src/lib.rs · wrote 2.0 KiB");
    }

    #[test]
    fn inspector_json_source_renders_content_instead_of_its_envelope() {
        let output = serde_json::json!({
            "content": "{\n  \"name\": \"orca\"\n}",
            "bytes": 20,
            "truncated": false
        });
        let (preview, omitted) = inspector_output_preview("anything", &output, Some("json"), false);
        assert_eq!(preview, "{\n  \"name\": \"orca\"\n}");
        assert!(!omitted);
    }

    #[test]
    fn inspector_source_output_reports_language_shape_and_size() {
        let tool = ToolActivity {
            call_id: "read-1".into(),
            call_line: "read_file src/main.rs".into(),
            tool_name: "read_file".into(),
            input: serde_json::json!({"path": "src/main.rs"}),
            started: Instant::now(),
            elapsed: Some(Duration::from_millis(1)),
            output: None,
            is_error: false,
            approval: None,
        };
        let output = serde_json::json!({
            "content": "fn main() {\n    println!(\"orca\");\n}\n",
            "bytes": 2048,
            "truncated": false
        });
        assert_eq!(
            inspector_code_facts(&tool, &output, Some("rust")).as_deref(),
            Some("rust · 3 lines · 2.0 KiB")
        );
    }

    #[test]
    fn inspector_write_input_renders_source_instead_of_escaped_json() {
        let tool = ToolActivity {
            call_id: "write-1".into(),
            call_line: "write_file README.md".into(),
            tool_name: "write_file".into(),
            input: serde_json::json!({
                "path": "README.md",
                "content": "# Orca\n\n    indented code\n"
            }),
            started: Instant::now(),
            elapsed: Some(Duration::from_millis(1)),
            output: Some(serde_json::json!({"path": "README.md", "bytesWritten": 26})),
            is_error: false,
            approval: None,
        };
        let inspector = flat_lines(&tool_inspector_lines(&tool, 80));
        assert!(inspector.contains("README.md · markdown · 3 lines · 26 B"));
        assert!(inspector.contains("# Orca"));
        assert!(inspector.contains("    indented code"));
        assert!(!inspector.contains("\\n"));
        assert!(inspector.contains("README.md · wrote 26 B"));
    }

    #[test]
    fn inspector_output_with_tabs_and_ansi_renders_clean_cells() {
        // du/ls emit tab-separated columns and some tools emit ANSI color;
        // raw control bytes in a cell desync the terminal cursor from the
        // draw buffer and leave ghost cells behind.
        let tool = ToolActivity {
            call_id: "shell-1".into(),
            call_line: "shell $ du -sh ~/.nvm/*".into(),
            tool_name: "shell".into(),
            input: serde_json::json!({"command": "du -sh ~/.nvm/*"}),
            started: Instant::now(),
            elapsed: Some(Duration::from_millis(1)),
            output: Some(serde_json::json!({
                "stdout": "205M\t/Users/akashswamy/.nvm/versions\n\u{1b}[31m12K\u{1b}[0m\t/tmp/x\n",
                "stderr": "",
                "exitCode": 0
            })),
            is_error: false,
            approval: None,
        };
        for line in tool_inspector_lines(&tool, 80) {
            for span in &line.spans {
                assert!(
                    !span.content.contains(|c: char| c.is_control()),
                    "control byte reached a cell: {:?}",
                    span.content
                );
            }
        }
    }

    #[test]
    fn inspector_json_preview_stops_after_one_level() {
        let output = serde_json::json!({
            "ok": true,
            "metadata": { "owner": { "name": "orca" }, "count": 3 },
            "results": [{ "id": 1 }, { "id": 2 }]
        });
        let (preview, omitted) = shallow_json_preview(&output);
        assert!(!omitted);
        assert!(preview.contains("\"ok\": true"));
        assert!(preview.contains("\"metadata\": { … }"));
        assert!(preview.contains("\"results\": [ … ]"));
        assert!(!preview.contains("owner"));
        assert!(!preview.contains("id"));
    }

    fn pending_texts(app: &App) -> Vec<String> {
        app.pending_history
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn notifications_use_the_shared_leading_glyph() {
        let _theme = THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let mut app = test_app();

        push_notice(&mut app, "theme set to default");

        let notice = app.pending_history.last().expect("notification line");
        assert_eq!(line_text(notice), "• theme set to default");
        assert_eq!(notice.spans[0].style, theme().accent);
        assert_eq!(notice.spans[1].style, theme().dim);
    }

    /// A refused command is still the system talking. Without the shared
    /// glyph it renders flush-left against the notices around it and
    /// reads as model output — so the glyph is the same and only the
    /// body carries the error color.
    #[test]
    fn errors_use_the_same_glyph_as_notices_with_an_error_body() {
        let _theme = THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let mut app = test_app();

        push_notice(&mut app, "plan mode · read-only");
        push_error(&mut app, "unknown mode: pkan");

        let notice = &app.pending_history[app.pending_history.len() - 2];
        let error = app.pending_history.last().expect("error line");
        assert_eq!(line_text(error), "• unknown mode: pkan");
        // Same leading glyph, so both lines start in the same column.
        assert_eq!(line_text(notice).chars().next(), Some('•'));
        assert_eq!(error.spans[0].style, notice.spans[0].style);
        // Severity is the body's job, and it differs from a notice.
        assert_eq!(error.spans[1].style, theme().error);
        assert_ne!(error.spans[1].style, notice.spans[1].style);
    }

    #[test]
    fn elapsed_labels_keep_sub_millisecond_tool_timings_visible() {
        assert_eq!(elapsed_label(Duration::ZERO), "0ns");
        assert_eq!(elapsed_label(Duration::from_nanos(850)), "850ns");
        assert_eq!(elapsed_label(Duration::from_micros(842)), "842µs");
        assert_eq!(elapsed_label(Duration::from_millis(14)), "14ms");
        assert_eq!(elapsed_label(Duration::from_millis(1_500)), "1.5s");
    }

    #[test]
    fn tool_results_connect_under_their_calls() {
        let mut app = test_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "shell".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            80,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "shell".into(),
                output: serde_json::json!({"stdout": "a.rs", "exitCode": 0}),
                is_error: false,
            },
            80,
        );
        let joined = flat_lines(&activity_lines(&app, 80, true));
        assert!(joined.contains("shell $ ls"));
        assert!(joined.contains("✓ shell $ ls · exit 0 · a.rs"));
    }

    fn flat_lines(lines: &[Line]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect()
    }

    #[test]
    fn streaming_reasoning_renders_inside_the_thinking_group() {
        let mut app = test_app();
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        app.reasoning = "secret chain of thought".into();

        let joined = flat_lines(&projected_transcript(&app, 80));
        assert!(
            joined.contains("Thinking"),
            "thinking group shown: {joined}"
        );
        assert!(
            joined.contains("secret chain of thought"),
            "reasoning tail shown: {joined}"
        );

        // Actual answer text still streams live.
        app.text = "partial answer".into();
        let joined = flat_lines(&projected_transcript(&app, 80));
        assert!(joined.contains("partial answer"));
    }

    #[test]
    fn thinking_joins_the_rail_and_the_expand_log() {
        let mut app = test_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ReasoningDelta {
                text: "let me check the file".into(),
            },
            80,
        );
        // A tool call ends the thinking phase even with no assistant text.
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "read_file".into(),
                input: serde_json::json!({"path": "a.rs"}),
            },
            80,
        );
        let joined = flat_lines(&activity_lines(&app, 80, true));
        assert!(
            joined.contains("Thinking"),
            "thinking group first: {joined}"
        );
        assert!(joined.contains("read_file a.rs"));
        let record = app.tool_log.last().expect("thinking recorded");
        assert_eq!(record.tool_name, "thinking");
        assert_eq!(record.output, serde_json::json!("let me check the file"));
        assert!(app.reasoning.is_empty(), "buffer reset after flush");
    }

    #[test]
    fn interrupted_thinking_still_lands_in_the_rail() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ReasoningDelta { text: "hmm".into() },
            80,
        );
        handle_ui_msg(&mut app, UiMsg::RunDone(Err("cancelled".into())), &tx, 80);
        let texts = pending_texts(&app);
        assert!(
            texts.iter().any(|t| t.contains("Thinking ·")),
            "partial thinking summarized: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("hmm")),
            "committed thinking collapsed"
        );
        assert_eq!(app.tool_log.last().unwrap().tool_name, "thinking");
        let details = flat_lines(&app.work_log.last().expect("work tree retained").lines);
        assert!(
            details.contains("Thinking"),
            "work tree retained: {details}"
        );
    }

    #[test]
    fn parallel_batch_results_are_labeled_with_their_tool() {
        let mut app = test_app();
        for (id, name, args) in [
            ("c1", "list_dir", serde_json::json!({"path": "."})),
            ("c2", "shell", serde_json::json!({"command": "git log"})),
        ] {
            handle_harness_event(
                &mut app,
                HarnessEvent::ToolCall {
                    tool_call_id: id.into(),
                    tool_name: name.into(),
                    input: args,
                },
                80,
            );
        }
        // Results complete out of call order.
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "list_dir".into(),
                output: serde_json::json!({"path": ".", "entries": ["a", "b"]}),
                is_error: false,
            },
            80,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "c2".into(),
                tool_name: "shell".into(),
                output: serde_json::json!({"stdout": "abc123", "exitCode": 0}),
                is_error: false,
            },
            80,
        );
        let joined = activity_lines(&app, 80, true)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let list_call = joined.find("list_dir .").unwrap();
        let list_result = joined.find("· 2 entries").unwrap();
        let shell_call = joined.find("shell $ git log").unwrap();
        let shell_result = joined.find("· exit 0 · abc123").unwrap();
        assert!(
            list_call < list_result && shell_call < shell_result,
            "results stay attached: {joined}"
        );

        // A later lone call joins the same work group without losing the
        // association between any earlier call and result.
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c3".into(),
                tool_name: "shell".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            80,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "c3".into(),
                tool_name: "shell".into(),
                output: serde_json::json!({"stdout": "", "exitCode": 0}),
                is_error: false,
            },
            80,
        );
        let joined = flat_lines(&activity_lines(&app, 80, true));
        assert!(joined.contains("shell $ ls"));
        assert!(joined.contains("✓ shell $ ls · exit 0"));
    }

    #[test]
    fn wrapped_user_prompts_carry_the_spine_on_every_line() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.composer = "alpha beta gamma delta epsilon zeta eta theta".into();
        submit(&mut app, &tx, 24);
        let texts = pending_texts(&app);
        let prompt_lines: Vec<&String> = texts.iter().filter(|t| t.starts_with("┃ ")).collect();
        assert!(prompt_lines.len() >= 2, "prompt should wrap: {texts:?}");
        for line in prompt_lines {
            assert!(line.starts_with("┃ "), "spine carried: {line}");
        }
    }

    #[test]
    fn bang_prompt_dispatches_a_shell_tool_in_the_workspace() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.composer = "!git status --short".into();
        app.cursor = app.composer.chars().count();

        submit(&mut app, &tx, 80);

        match rx.try_recv() {
            Ok(WorkerCmd::Shell {
                command,
                working_dir,
                ..
            }) => {
                assert_eq!(command, "git status --short");
                assert_eq!(working_dir, app.cfg.workspace_root);
            }
            other => panic!("expected shell command, got {:?}", other.is_ok()),
        }
        assert!(app.running());
        assert!(app.composer.is_empty());
        assert!(pending_texts(&app)
            .join("\n")
            .contains("!git status --short"));
    }

    #[test]
    fn queued_bang_prompt_stays_a_shell_command() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        app.prompt_queue.push_back("!pwd".into());

        handle_ui_msg(&mut app, UiMsg::RunDone(Ok(String::new())), &tx, 80);

        assert!(matches!(
            rx.try_recv(),
            Ok(WorkerCmd::Shell { command, .. }) if command == "pwd"
        ));
    }

    #[test]
    fn shell_done_settles_the_tool_and_resets_the_run() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "user-shell-1".into(),
                tool_name: "shell".into(),
                input: serde_json::json!({"command": "pwd"}),
            },
            80,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "user-shell-1".into(),
                tool_name: "shell".into(),
                output: serde_json::json!({"stdout": "/test-ws\n", "exitCode": 0}),
                is_error: false,
            },
            80,
        );

        handle_ui_msg(&mut app, UiMsg::ShellDone, &tx, 80);

        assert!(!app.running());
        assert_eq!(
            app.tool_log.last().map(|tool| tool.tool_name.as_str()),
            Some("shell")
        );
    }

    #[test]
    fn prompts_submitted_while_running_queue_in_fifo_order() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };

        app.composer = "add queue rendering tests".into();
        submit(&mut app, &tx, 80);
        app.composer = "update the readme".into();
        submit(&mut app, &tx, 80);

        assert_eq!(
            app.prompt_queue.iter().cloned().collect::<Vec<_>>(),
            vec!["add queue rendering tests", "update the readme"]
        );
        assert!(app.composer.is_empty(), "queued input clears the composer");
        assert!(
            rx.try_recv().is_err(),
            "queued turns do not overlap the run"
        );
    }

    #[test]
    fn successful_run_starts_the_next_queued_prompt() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        app.prompt_queue.extend([
            "add queue rendering tests".to_string(),
            "update the readme".to_string(),
        ]);

        handle_ui_msg(&mut app, UiMsg::RunDone(Ok(String::new())), &tx, 80);

        match rx.try_recv() {
            Ok(WorkerCmd::Run { prompt, .. }) => {
                assert_eq!(prompt, "add queue rendering tests")
            }
            other => panic!("expected queued run, got {:?}", other.is_ok()),
        }
        assert!(app.running());
        assert_eq!(
            app.prompt_queue.iter().cloned().collect::<Vec<_>>(),
            vec!["update the readme"]
        );
        assert!(
            pending_texts(&app)
                .iter()
                .any(|line| line.contains("add queue rendering tests")),
            "a queued prompt enters the transcript when it starts"
        );
    }

    /// A finished turn reports its wall time and how many tool calls it made;
    /// a failed or interrupted run does not.
    #[test]
    fn run_done_reports_turn_duration_and_tool_calls() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        for id in ["c1", "c2"] {
            handle_harness_event(
                &mut app,
                HarnessEvent::ToolCall {
                    tool_call_id: id.into(),
                    tool_name: "shell".into(),
                    input: serde_json::json!({"command": "true"}),
                },
                80,
            );
            handle_harness_event(
                &mut app,
                HarnessEvent::ToolResult {
                    tool_call_id: id.into(),
                    tool_name: "shell".into(),
                    output: serde_json::json!(""),
                    is_error: false,
                },
                80,
            );
        }
        handle_ui_msg(&mut app, UiMsg::RunDone(Ok(String::new())), &tx, 80);
        let summary = app.last_turn_summary.clone().expect("summary recorded");
        assert!(summary.starts_with("Turn took"), "{summary}");
        assert!(summary.contains("s and took 2 tool calls"), "{summary}");
        assert!(
            !pending_texts(&app).iter().any(|t| t.contains("Turn took")),
            "summary stays out of the transcript"
        );
        let rail: Vec<String> = live_lines(&app, 80)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            rail.iter().any(|t| t.contains(&summary)),
            "summary rendered in the rail above the composer: {rail:?}"
        );

        let mut failed = test_app();
        failed.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        handle_ui_msg(
            &mut failed,
            UiMsg::RunDone(Err("cancelled".into())),
            &tx,
            80,
        );
        assert!(
            failed.last_turn_summary.is_none(),
            "no summary on an interrupted run"
        );
    }

    #[test]
    fn failed_run_pauses_the_queue_until_empty_enter_resumes_it() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        app.prompt_queue.push_back("inspect the failure".into());

        handle_ui_msg(
            &mut app,
            UiMsg::RunDone(Err("model endpoint unavailable".into())),
            &tx,
            80,
        );

        assert!(!app.running());
        assert_eq!(
            app.prompt_queue.front().map(String::as_str),
            Some("inspect the failure")
        );
        assert!(
            rx.try_recv().is_err(),
            "failure must not cascade through the queue"
        );

        app.composer.clear();
        submit(&mut app, &tx, 80);
        match rx.try_recv() {
            Ok(WorkerCmd::Run { prompt, .. }) => assert_eq!(prompt, "inspect the failure"),
            other => panic!("expected resumed run, got {:?}", other.is_ok()),
        }
        assert!(app.prompt_queue.is_empty());
        assert!(app.running());
    }

    #[test]
    fn queue_rail_previews_three_prompts_and_collapses_overflow() {
        let mut app = test_app();
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        app.prompt_queue.extend([
            "first queued prompt".to_string(),
            "second queued prompt".to_string(),
            "third queued prompt".to_string(),
            "fourth queued prompt".to_string(),
        ]);

        let rows = live_lines(&app, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(rows[0].contains("queued · 4"), "queue heading: {rows:?}");
        assert!(rows[1].contains("next") && rows[1].contains("first queued prompt"));
        assert!(rows[2].contains("2") && rows[2].contains("second queued prompt"));
        assert!(rows[3].contains("3") && rows[3].contains("third queued prompt"));
        assert!(rows[4].contains("+1 more"), "overflow summary: {rows:?}");
        assert!(
            rows[5].contains("working"),
            "spinner follows queue: {rows:?}"
        );
    }

    #[test]
    fn paused_queue_shows_resume_guidance_in_the_composer_and_status() {
        let mut app = App::new(TuiConfig {
            model_name: "test".into(),
            workspace_name: "/workspace".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        });
        app.prompt_queue.push_back("inspect the failure".into());

        let screen = rendered_rows(&mut app, 100, 24).join("\n");
        assert!(screen.contains("queue paused · enter to resume"));
        assert!(screen.contains("queued · 1"));
        assert!(screen.contains("queued 1 · enter resume · /queue clear"));
    }

    #[test]
    fn queue_clear_discards_waiting_prompts_without_interrupting_the_run() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        app.prompt_queue.extend(["one".into(), "two".into()]);
        app.composer = "/queue clear".into();

        submit(&mut app, &tx, 80);

        assert!(app.prompt_queue.is_empty());
        assert!(
            app.running(),
            "clearing the queue leaves the current run alone"
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn compact_command_reaches_the_worker_and_reports_the_result() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.composer = "/compact".into();
        submit(&mut app, &tx, 80);
        assert!(
            matches!(rx.try_recv(), Ok(WorkerCmd::Compact)),
            "/compact sends the worker command"
        );

        let report = orca_harness_extensions::CompactReport {
            messages_before: 41,
            messages_after: 2,
            bytes_before: 130_574,
            bytes_after: 1_264,
            est_tokens_before: 32_643,
            est_tokens_after: 316,
            head_messages: 40,
            tail_messages: 0,
            elided_results: 16,
            elided_bytes: 116_177,
            elided_call_ids: vec!["c1".into()],
            summary: "summary".into(),
            files_read: vec![],
            files_modified: vec![],
        };
        app.context_tokens = 32_643;
        handle_ui_msg(&mut app, UiMsg::Compacted(Ok(report)), &tx, 80);
        let text = app
            .pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("compacted: 41 -> 2 messages"), "{text}");
        assert!(text.contains("16 tool outputs"), "{text}");
        assert!(text.contains("recoverable via read_tool_result"), "{text}");
        assert_eq!(
            app.context_tokens, 316,
            "the status-line context meter reflects the compacted size"
        );
    }

    #[test]
    fn context_meter_tracks_the_latest_step_not_the_session_total() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        for (input, output) in [(1_000, 50), (1_200, 80)] {
            handle_ui_msg(
                &mut app,
                UiMsg::Event(HarnessEvent::Usage {
                    usage: orca_harness_core::Usage {
                        input_tokens: input,
                        output_tokens: output,
                        cache_read_tokens: 0,
                        cache_create_tokens: 0,
                    },
                }),
                &tx,
                80,
            );
        }
        assert_eq!(app.tokens_in, 2_200, "session total accumulates");
        assert_eq!(
            app.context_tokens, 1_280,
            "context meter is the latest step's input + output"
        );

        // A tool result lands before the next model step: pi-style
        // trailing estimate (bytes/4) until real usage overwrites it.
        let output = serde_json::json!({"content": "x".repeat(396)});
        let bytes = serde_json::to_string(&output).unwrap().len() as u64;
        handle_ui_msg(
            &mut app,
            UiMsg::Event(HarnessEvent::ToolResult {
                tool_call_id: "c9".into(),
                tool_name: "read_file".into(),
                output,
                is_error: false,
            }),
            &tx,
            80,
        );
        assert_eq!(app.context_tokens, 1_280 + bytes / 4);
    }

    #[test]
    fn context_segment_shows_percentage_when_the_window_is_known() {
        assert_eq!(context_segment(41_881, Some(128_000)), "ctx 32%");
        assert_eq!(context_segment(500, None), "ctx ~500");
        assert_eq!(context_segment(2_350, Some(0)), "ctx ~2.4k");
    }

    #[test]
    fn slash_usage_opens_a_read_only_tray_and_esc_closes_it() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.tokens_in = 250_798;
        app.tokens_out = 3_609;
        app.cache_read_total = 12;
        app.cache_write_total = 3;
        app.usage_steps = 6;
        app.context_tokens = 41_881;
        app.context_window = Some(128_000);
        slash_command(&mut app, "usage", &tx, 100);
        assert!(matches!(app.overlay, Some(Overlay::Usage)));

        let text = flat_lines(&live_lines(&app, 100));
        assert!(text.contains("Session usage"), "{text}");
        assert!(text.contains("41881 / 128000 (32%)"), "{text}");
        assert!(text.contains("input        250798"), "{text}");
        assert!(text.contains("cache read   12"), "{text}");
        assert!(text.contains("total        254422"), "{text}");
        assert!(text.contains("model steps  6"), "{text}");

        press(&mut app, &tx, KeyCode::Esc);
        assert!(app.overlay.is_none(), "esc dismisses the tray");
        assert!(rx.try_recv().is_err(), "the tray never talks to the worker");
    }

    #[test]
    fn later_turns_have_no_divider_or_trailing_spine() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();

        app.composer = "first turn".into();
        submit(&mut app, &tx, 40);
        app.run = RunState::Idle;
        app.composer = "second turn".into();
        submit(&mut app, &tx, 40);

        let texts = pending_texts(&app);
        assert!(
            !texts.iter().any(|line| line.starts_with("  ─")),
            "turn divider removed: {texts:?}"
        );
        assert!(
            !texts.iter().any(|line| line == "┃"),
            "spine ends with prompt text: {texts:?}"
        );
        let second = texts
            .iter()
            .position(|line| line == "┃ second turn")
            .expect("second prompt");
        assert_eq!(texts[second - 1], "", "one row separates turns: {texts:?}");
        assert!(
            second < 2 || !texts[second - 2].is_empty(),
            "spacing stays to one row: {texts:?}"
        );
        assert_eq!(app.turn_count, 2);
    }

    #[test]
    fn approval_verdicts_align_under_the_call() {
        let mut app = test_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "shell".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            80,
        );
        let (respond, _rx) = tokio::sync::oneshot::channel();
        app.approval = Some(crate::msg::ApprovalRequest {
            tool_name: "shell".into(),
            detail: "shell $ ls".into(),
            respond,
        });
        handle_approval_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        );
        let joined = flat_lines(&activity_lines(&app, 80, true));
        assert!(joined.contains("shell $ ls"));
        assert!(joined.contains("□ shell $ ls · approved"));
    }

    #[test]
    fn clear_flushes_the_screen_and_resets_session_state() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.scroll = 20;
        app.tokens_in = 100;
        app.spinner_frame = 99;
        app.prompt_queue.push_back("waiting prompt".into());
        app.tool_log.push(ToolRecord {
            call_line: "shell $ ls".into(),
            tool_name: "shell".into(),
            output: serde_json::json!({}),
            inner: Vec::new(),
        });

        slash_command(&mut app, "clear", &tx, 80);

        assert!(app.transcript.is_empty(), "transcript wiped");
        assert_eq!(app.scroll, 0);
        assert_eq!(app.tokens_in, 0);
        assert!(app.prompt_queue.is_empty(), "prompt queue wiped");
        assert!(app.tool_log.is_empty(), "expandable log wiped");
        assert!(
            matches!(rx.try_recv(), Ok(WorkerCmd::Clear)),
            "worker told to reset the context"
        );
        assert!(
            app.pending_history.is_empty(),
            "clear leaves an empty transcript"
        );
        let screen = rendered_rows(&mut app, 80, 24).join("\n");
        assert!(
            !screen.contains("ORCA HARNESS"),
            "welcome should stay removed: {screen}"
        );
    }

    #[test]
    fn other_mouse_events_are_ignored() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        handle_terminal_event(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left)),
            &tx,
            80,
        );
        assert_eq!(app.scroll, 0);
        assert!(app.composer.is_empty());
    }

    #[test]
    fn live_activity_groups_thinking_and_parallel_tools() {
        let mut app = test_app();
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        handle_harness_event(
            &mut app,
            HarnessEvent::ReasoningDelta {
                text: "Inspecting the event flow".into(),
            },
            100,
        );
        for (id, name, input) in [
            ("c1", "read_file", serde_json::json!({"path": "src/tui.rs"})),
            ("c2", "shell", serde_json::json!({"command": "cargo test"})),
        ] {
            handle_harness_event(
                &mut app,
                HarnessEvent::ToolCall {
                    tool_call_id: id.into(),
                    tool_name: name.into(),
                    input,
                },
                100,
            );
        }
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "read_file".into(),
                output: serde_json::json!({"bytes": 2048, "content": "..."}),
                is_error: false,
            },
            100,
        );

        let joined = projected_transcript(&app, 100)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("Thinking"),
            "thinking group missing: {joined}"
        );
        assert!(
            joined.contains("Work · ✓ 1 · □ 1"),
            "work totals missing: {joined}"
        );
        assert!(
            joined.contains("read_file src/tui.rs"),
            "completed call missing: {joined}"
        );
        assert!(
            joined.contains("✓ read_file src/tui.rs · read 2048 bytes"),
            "result missing: {joined}"
        );
        assert!(
            joined.contains("shell $ cargo test"),
            "running call missing: {joined}"
        );
        assert!(joined.contains("□"), "running state missing: {joined}");
    }

    #[test]
    fn live_activity_prioritizes_running_tools_and_bounds_the_history() {
        let mut app = test_app();
        for index in 0..12 {
            app.activity_tools.push(ToolActivity {
                call_id: format!("call-{index}"),
                call_line: format!("read_file file-{index}.rs"),
                tool_name: "read_file".into(),
                input: serde_json::json!({"path": format!("file-{index}.rs")}),
                started: Instant::now(),
                elapsed: Some(Duration::from_millis(1)),
                output: Some(serde_json::json!({"bytes": 42})),
                is_error: false,
                approval: None,
            });
        }
        app.activity_tools.push(ToolActivity {
            call_id: "call-shell".into(),
            call_line: "shell $ cargo test --workspace".into(),
            tool_name: "shell".into(),
            input: serde_json::json!({"command": "cargo test --workspace"}),
            started: Instant::now(),
            elapsed: None,
            output: None,
            is_error: false,
            approval: None,
        });

        let rendered = activity_lines(&app, 100, true);
        let joined = rendered
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("… 5 earlier tools"),
            "history summarized: {joined}"
        );
        assert!(
            joined.contains("□ shell $ cargo test --workspace"),
            "running tool retained: {joined}"
        );
        assert!(
            !joined.contains("file-0.rs"),
            "oldest tools hidden: {joined}"
        );
        assert!(rendered.len() <= LIVE_TOOL_ROWS + 2, "rail stays bounded");
    }

    #[test]
    fn completed_run_keeps_activity_expanded_before_the_answer() {
        let mut app = test_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ReasoningDelta {
                text: "private reasoning text".into(),
            },
            100,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "shell".into(),
                input: serde_json::json!({"command": "cargo test"}),
            },
            100,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "shell".into(),
                output: serde_json::json!({"stdout": "42 tests passed", "exitCode": 0}),
                is_error: false,
            },
            100,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::Result {
                message: "Everything passed.".into(),
            },
            100,
        );

        let texts = pending_texts(&app);
        let joined = texts.join("\n");
        let work = joined.find("Work · 1 tool").expect("work rail");
        let answer = joined.find("Everything passed.").expect("answer");
        assert!(work < answer, "work precedes answer: {joined}");
        assert!(joined.contains("shell $ cargo test"));
        assert!(joined.contains("✓ shell $ cargo test · exit 0 · 42 tests passed"));
        assert!(
            !joined.contains("private reasoning text"),
            "completed thinking is collapsed"
        );

        let details = flat_lines(&app.work_log.last().expect("work tree retained").lines);
        assert!(details.contains("Thinking"));
        assert!(details.contains("shell $ cargo test"));
        assert!(details.contains("✓ shell $ cargo test · exit 0 · 42 tests passed"));
        assert!(
            app.work_log.last().expect("work tree retained").expanded,
            "completed rails are expanded by default"
        );

        app.absorb_pending();
        assert!(expand_latest_work(&mut app));
        let expanded = app
            .transcript
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(expanded.contains("shell $ cargo test"));
        let tool = expanded.find("shell $ cargo test").expect("expanded tool");
        let answer = expanded.find("Everything passed.").expect("answer");
        assert!(tool < answer, "work expands in place: {expanded}");
        let once = app.transcript.len();
        assert!(expand_latest_work(&mut app));
        assert_eq!(app.transcript.len(), once, "repeat expansion is a no-op");
    }

    #[test]
    fn transcript_preserves_model_tool_phase_chronology() {
        let mut app = test_app();

        handle_harness_event(
            &mut app,
            HarnessEvent::ReasoningDelta {
                text: "inspect the extension trait".into(),
            },
            100,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::Assistant {
                message: "I understand the core mechanism. I will inspect the built-ins.\n\n"
                    .into(),
            },
            100,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "read_file".into(),
                input: serde_json::json!({"path": "crates/extensions/src/lib.rs"}),
            },
            100,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "read_file".into(),
                output: serde_json::json!({"bytes": 2048}),
                is_error: false,
            },
            100,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ReasoningDelta {
                text: "compare the concrete implementations".into(),
            },
            100,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::Result {
                message: "The extension mechanism is useful.".into(),
            },
            100,
        );

        let texts = pending_texts(&app);
        let joined = texts.join("\n");
        let first_thinking = joined.find("Thinking ·").expect("first thinking phase");
        let checkpoint = joined
            .find("I understand the core mechanism")
            .expect("checkpoint");
        let work = joined.find("Work · 1 tool").expect("tool phase");
        let second_thinking = joined
            .match_indices("Thinking ·")
            .nth(1)
            .map(|(index, _)| index)
            .expect("second thinking phase");
        let answer = joined
            .find("The extension mechanism is useful.")
            .expect("answer");

        assert!(
            first_thinking < checkpoint
                && checkpoint < work
                && work < second_thinking
                && second_thinking < answer,
            "event chronology retained: {joined}"
        );
        let checkpoint_row = texts
            .iter()
            .position(|line| line.contains("I understand the core mechanism"))
            .expect("checkpoint row");
        let second_thinking_row = texts
            .iter()
            .rposition(|line| line.contains("Thinking ·"))
            .expect("second thinking row");
        assert!(
            texts[checkpoint_row + 1..=second_thinking_row]
                .iter()
                .all(|line| !line.is_empty()),
            "work phases do not inject blank rows: {texts:?}"
        );
        assert_eq!(app.work_log.len(), 3, "each phase remains expandable");
    }

    #[test]
    fn transcript_block_component_owns_vertical_rhythm() {
        let mut app = test_app();
        app.push_line(Line::from("prompt"));

        app.push_markdown_block(
            "\nfirst paragraph\n\nsecond paragraph\n\n",
            80,
            BlockSpacing::Tight,
        );
        app.push_transcript_block(
            vec![Line::from(""), Line::from("work"), Line::from("")],
            BlockSpacing::Tight,
        );
        app.push_markdown_block("final answer\n", 80, BlockSpacing::Section);

        assert_eq!(
            pending_texts(&app),
            vec![
                "prompt",
                "",
                "  first paragraph",
                "",
                "  second paragraph",
                "work",
                "",
                "  final answer",
            ]
        );
    }

    #[test]
    fn transcript_has_no_role_labels() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.composer = "Explain the change".into();
        submit(&mut app, &tx, 80);
        handle_harness_event(
            &mut app,
            HarnessEvent::Result {
                message: "Here is the change.".into(),
            },
            80,
        );
        let joined = pending_texts(&app).join("\n");
        let prompt = joined.find("Explain the change").expect("prompt");
        let answer = joined.find("Here is the change.").expect("answer");
        assert!(prompt < answer, "turn order: {joined}");
        assert!(!joined.contains("YOU"), "user label removed: {joined}");
        assert!(
            !joined.contains("ORCA"),
            "assistant label removed: {joined}"
        );
    }

    #[test]
    fn edit_activity_keeps_its_diff_preview() {
        let mut app = test_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "edit_file".into(),
                input: serde_json::json!({"path": "src/lib.rs", "old": "let x = 1;", "new": "let x = 2;"}),
            },
            100,
        );
        let joined = flat_lines(&activity_lines(&app, 100, true));
        assert!(
            joined.contains("- let x = 1;"),
            "removed line visible: {joined}"
        );
        assert!(
            joined.contains("+ let x = 2;"),
            "added line visible: {joined}"
        );
    }

    #[test]
    fn selected_tool_keeps_elapsed_time_next_to_the_call() {
        let mut app = test_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "shell".into(),
                input: serde_json::json!({"command": "cargo test --workspace"}),
            },
            180,
        );

        let joined = flat_lines(&activity_lines_selected(&app, 180, true, Some(0)));
        assert!(
            joined.contains("shell $ cargo test --workspace · "),
            "elapsed follows the call without an alignment gap: {joined}"
        );
    }

    #[test]
    fn multiline_edit_preview_is_source_shaped_and_bounded() {
        let mut app = test_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "edit_file".into(),
                input: serde_json::json!({
                    "path": "src/lib.rs",
                    "old": "fn old() {\n    one();\n    two();\n    three();\n}",
                    "new": "fn new() {\n    four();\n    five();\n}"
                }),
            },
            100,
        );

        let rendered = activity_lines(&app, 100, true);
        let joined = flat_lines(&rendered);
        assert!(joined.contains("- fn old() {"), "old source: {joined}");
        assert!(joined.contains("-     one();"), "indentation: {joined}");
        assert!(joined.contains("+ fn new() {"), "new source: {joined}");
        assert!(
            joined.contains("… 3 more changed lines"),
            "bounded preview: {joined}"
        );
        assert_eq!(
            rendered
                .iter()
                .filter(|line| {
                    let text = line
                        .spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>();
                    text.contains(" - ") || text.contains(" + ")
                })
                .count(),
            6,
            "only the preview budget is rendered"
        );
    }

    #[test]
    fn failed_tool_expands_its_output_in_the_rail() {
        let mut app = test_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "shell".into(),
                input: serde_json::json!({"command": "cargo test"}),
            },
            100,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "shell".into(),
                output: serde_json::json!({"stdout": "test result: FAILED", "stderr": "assertion failed", "exitCode": 1}),
                is_error: true,
            },
            100,
        );
        let joined = flat_lines(&activity_lines(&app, 100, true));
        assert!(
            joined.contains("× shell $ cargo test · exit 1"),
            "failure state shown: {joined}"
        );
        assert!(
            joined.contains("test result: FAILED"),
            "stdout expanded: {joined}"
        );
        assert!(
            joined.contains("assertion failed"),
            "stderr expanded: {joined}"
        );
    }

    #[test]
    fn immediate_model_failure_renders_without_assistant_label() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        handle_ui_msg(
            &mut app,
            UiMsg::RunDone(Err("model endpoint unavailable".into())),
            &tx,
            80,
        );
        let joined = pending_texts(&app).join("\n");
        assert!(joined.contains("run failed:"), "failure message: {joined}");
        assert!(
            !joined.contains("ORCA"),
            "assistant label removed: {joined}"
        );
    }

    fn rendered_rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn bang_composer_keeps_the_original_unfilled_style() {
        let mut app = test_app();
        app.composer = "!echo hello".into();
        app.cursor = app.composer.chars().count();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, 22)].bg, ratatui::style::Color::Reset);
        assert_ne!(buffer[(0, 20)].symbol(), "┌");
    }

    #[test]
    fn empty_session_has_a_useful_static_welcome() {
        let mut app = App::new(TuiConfig {
            model_name: "gpt-oss:20b".into(),
            workspace_name: "/workspace/orca-harness".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        });
        let screen = rendered_rows(&mut app, 90, 30).join("\n");

        assert!(screen.contains("▀▄ ORCACODE"), "logo missing: {screen}");
        assert!(
            screen.contains(concat!("v", env!("CARGO_PKG_VERSION"))),
            "version missing: {screen}"
        );
        assert!(screen.contains("gpt-oss:20b"), "model missing: {screen}");
        assert!(
            screen.contains("/workspace/orca-harness"),
            "workspace missing: {screen}"
        );
        assert!(
            screen.contains("Describe a task to begin"),
            "welcome hint missing: {screen}"
        );
        assert!(screen.contains("/models switch model"));
    }

    #[test]
    fn welcome_centres_the_visible_card_not_its_maximum_width() {
        let mut app = test_app();
        let rows = rendered_rows(&mut app, 90, 30);
        let subtitle = rows
            .iter()
            .find(|row| row.contains("A small, fast agent runtime"))
            .expect("welcome subtitle");
        let visible_width = "A small, fast agent runtime for your terminal"
            .chars()
            .count();

        assert_eq!(
            subtitle.chars().take_while(|ch| *ch == ' ').count(),
            (90 - visible_width) / 2,
            "the longest visible row defines the card centre: {subtitle:?}"
        );
    }

    #[test]
    fn startup_notices_stay_behind_the_welcome_until_the_first_turn() {
        let mut app = test_app();
        push_notice(&mut app, "MCP docs connected · 4 tools");
        app.absorb_pending();

        let screen = rendered_rows(&mut app, 90, 30).join("\n");

        assert!(screen.contains("▀▄ ORCACODE"), "welcome missing: {screen}");
        assert!(
            !screen.contains("MCP docs connected"),
            "startup notice should remain in the background: {screen}"
        );
        assert!(
            flat_lines(&app.transcript).contains("MCP docs connected"),
            "startup notice should remain recorded"
        );
    }

    /// The rendered status line, which is the row carrying the model name.
    fn status_row(app: &mut App) -> String {
        rendered_rows(app, 100, 24)
            .into_iter()
            .find(|row| row.contains(&app.cfg.model_name))
            .expect("status line")
    }

    #[test]
    fn copying_the_inspected_tool_leaves_the_transcript_out() {
        let mut app = test_app();
        app.transcript
            .push(Line::from("the answer text in the left pane"));
        app.last_answer = Some("the answer text in the left pane".into());
        app.activity_tools.push(ToolActivity {
            call_id: "call-shell".into(),
            call_line: "shell $ cargo run --release -p orcacode".into(),
            tool_name: "shell".into(),
            input: serde_json::json!({"command": "cargo run --release -p orcacode"}),
            started: Instant::now(),
            elapsed: None,
            output: Some(serde_json::json!({"text": "compiling"})),
            is_error: false,
            approval: None,
        });
        app.split_tool = Some(0);

        copy_command(&mut app, "tool");
        let copied = app.clipboard_pending.take().expect("tool copy");

        assert!(
            copied.contains("cargo run --release -p orcacode") && copied.contains("compiling"),
            "inspector pane should come out whole: {copied}"
        );
        assert!(
            !copied.contains("the answer text in the left pane"),
            "the other pane should stay out of it: {copied}"
        );
    }

    #[test]
    fn scrolling_offers_the_selection_hint_then_settles_back() {
        let mut app = test_app();
        app.turn_count = 1;
        for row in 0..40 {
            app.transcript.push(Line::from(format!("row {row}")));
        }

        scroll_transcript(&mut app, 5);
        let status = status_row(&mut app);
        assert!(
            status.contains("opt/shift+drag selects") && status.contains("ctrl+y copies"),
            "fresh scroll should offer both ways out: {status}"
        );

        // Past its window the hint gives the status line back.
        app.scroll_hint_at = Some(Instant::now() - SCROLL_HINT - Duration::from_secs(1));
        let status = status_row(&mut app);
        assert!(
            status.contains("scrolled · pgdn to follow"),
            "hint should settle back: {status}"
        );
        assert!(!app.scroll_hint_live(), "expired hint should stop the tick");
    }

    #[test]
    fn status_line_starts_with_model_and_ends_with_workspace_name() {
        let mut app = App::new(TuiConfig {
            model_name: "gpt-oss:20b".into(),
            workspace_name: "/workspace/orca-harness".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        });

        let rows = rendered_rows(&mut app, 100, 24);
        let status = rows
            .iter()
            .find(|row| row.contains("idle"))
            .expect("status line");

        assert!(
            status.starts_with(" gpt-oss:20b ·"),
            "model not first: {status}"
        );
        assert!(
            status.ends_with("· orca-harness"),
            "workspace not last: {status}"
        );
        assert!(
            !status.contains("cwd"),
            "cwd prefix should be omitted: {status}"
        );
        assert!(
            !status.contains("/workspace/"),
            "full path should be omitted: {status}"
        );
    }

    #[test]
    fn assistant_deltas_render_in_the_transcript_above_live_status() {
        let mut app = App::new(TuiConfig {
            model_name: "test".into(),
            workspace_name: "/workspace".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        });
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        handle_harness_event(
            &mut app,
            HarnessEvent::AssistantDelta {
                text: "streamed answer".into(),
            },
            80,
        );

        let rows = rendered_rows(&mut app, 80, 24);
        let answer_row = rows
            .iter()
            .position(|row| row.contains("streamed answer"))
            .expect("streamed answer rendered");
        let status_row = rows
            .iter()
            .position(|row| row.contains("writing"))
            .expect("live status rendered");
        assert!(
            answer_row < status_row,
            "answer belongs to transcript above live status: {rows:#?}"
        );
    }

    #[test]
    fn composer_has_one_blank_row_above_it_without_a_divider() {
        let mut app = App::new(TuiConfig {
            model_name: "test".into(),
            workspace_name: "/workspace".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        });
        app.transcript.push(Line::from("final answer"));

        let rows = rendered_rows(&mut app, 80, 10);
        let composer = rows
            .iter()
            .position(|row| row.contains("ask anything"))
            .expect("composer");
        assert!(rows[composer - 1].is_empty(), "gap has no divider");
    }

    #[test]
    fn assistant_deltas_are_markdown_rendered_while_streaming() {
        let mut app = App::new(TuiConfig {
            model_name: "test".into(),
            workspace_name: "/workspace".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        });
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        handle_harness_event(
            &mut app,
            HarnessEvent::AssistantDelta {
                text: "# Streaming heading".into(),
            },
            80,
        );

        let joined = rendered_rows(&mut app, 80, 24).join("\n");
        assert!(joined.contains("Streaming heading"));
        assert!(
            !joined.contains("# Streaming heading"),
            "markdown syntax is rendered, not printed raw: {joined}"
        );
    }

    #[test]
    fn completed_assistant_event_does_not_blank_the_stream_before_result() {
        let mut app = App::new(TuiConfig {
            model_name: "test".into(),
            workspace_name: "/workspace".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        });
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        handle_harness_event(
            &mut app,
            HarnessEvent::AssistantDelta {
                text: "continuous answer".into(),
            },
            80,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::Assistant {
                message: "continuous answer".into(),
            },
            80,
        );

        let joined = rendered_rows(&mut app, 80, 24).join("\n");
        assert!(
            joined.contains("continuous answer"),
            "completed event stays projected until result: {joined}"
        );
    }

    fn press(app: &mut App, tx: &mpsc::UnboundedSender<WorkerCmd>, code: KeyCode) {
        handle_terminal_event(
            app,
            CtEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            tx,
            80,
        );
    }

    fn catalog() -> Vec<ModelInfo> {
        ["acme/fast-1", "acme/smart-9", "other/tiny"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                name: None,
                context_length: Some(32_000),
                pricing: None,
            })
            .collect()
    }

    #[test]
    fn at_opens_the_standard_location_picker_and_filters_as_you_type() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        press(&mut app, &tx, KeyCode::Char('@'));
        let Some(Overlay::Locations(location)) = app.overlay.as_mut() else {
            panic!("expected @ to open the location picker");
        };
        location.entries = vec![
            LocationEntry {
                path: "crates/cli/src/tui.rs".into(),
                directory: false,
            },
            LocationEntry {
                path: "README.md".into(),
                directory: false,
            },
        ];
        location.sync_len();

        press(&mut app, &tx, KeyCode::Char('t'));

        let Some(Overlay::Locations(location)) = &app.overlay else {
            panic!("location picker should remain open");
        };
        assert_eq!(app.composer, "@t");
        assert_eq!(location.query, "t");
        assert_eq!(location.filtered().len(), 1);
    }

    #[test]
    fn backspace_cancels_an_empty_location_picker_and_removes_the_at() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        press(&mut app, &tx, KeyCode::Char('@'));

        press(&mut app, &tx, KeyCode::Backspace);

        assert!(app.overlay.is_none());
        assert_eq!(app.composer, "");
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn delete_cancels_an_empty_location_picker_and_removes_the_at() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        press(&mut app, &tx, KeyCode::Char('@'));

        press(&mut app, &tx, KeyCode::Delete);

        assert!(app.overlay.is_none());
        assert_eq!(app.composer, "");
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn tab_inserts_the_selected_folder_mention() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.composer = "work in @cr".into();
        app.cursor = app.composer.chars().count();
        app.overlay = Some(Overlay::Locations(LocationPicker {
            entries: vec![LocationEntry {
                path: "crates/cli".into(),
                directory: true,
            }],
            query: "cr".into(),
            token_start: 8,
            picker: ListPicker::new(1),
        }));

        press(&mut app, &tx, KeyCode::Tab);

        assert!(app.overlay.is_none());
        assert_eq!(app.composer, "work in @crates/cli/ ");
        assert_eq!(app.cursor, app.composer.chars().count());
    }

    #[test]
    fn backspace_removes_an_inserted_location_mention_in_one_go() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.composer = "work in @crates/cli/ ".into();
        app.cursor = app.composer.chars().count();

        press(&mut app, &tx, KeyCode::Backspace);

        assert_eq!(app.composer, "work in ");
        assert_eq!(app.cursor, app.composer.chars().count());
    }

    #[test]
    fn submitting_a_mention_sends_a_plain_path_but_shows_the_at() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.composer = "read @docs/crate-diagram.md please".into();
        app.cursor = app.composer.chars().count();

        submit(&mut app, &tx, 80);

        match rx.try_recv() {
            Ok(WorkerCmd::Run { prompt, .. }) => {
                assert_eq!(prompt, "read docs/crate-diagram.md please");
            }
            other => panic!("expected a run, got {:?}", other.is_ok()),
        }
        assert_eq!(
            app.prompt_history.last().map(String::as_str),
            Some("read @docs/crate-diagram.md please"),
            "recall keeps what the user typed"
        );
    }

    #[test]
    fn addresses_and_bare_at_signs_survive_submission() {
        assert_eq!(
            strip_location_mentions("mail dev@example.com about @user@host and @ 5pm"),
            "mail dev@example.com about @user@host and @ 5pm"
        );
    }

    #[test]
    fn at_inside_a_word_does_not_open_the_location_picker() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.composer = "user".into();
        app.cursor = 4;

        press(&mut app, &tx, KeyCode::Char('@'));

        assert!(app.overlay.is_none());
        assert_eq!(app.composer, "user@");
    }

    #[test]
    fn catalog_reply_opens_the_picker_seeded_with_the_command_filter() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.picker_pending = Some("acme".into());
        handle_ui_msg(&mut app, UiMsg::Models(Ok(catalog())), &tx, 80);
        let Some(Overlay::Models(picker)) = &app.overlay else {
            panic!("expected the model picker to open");
        };
        assert_eq!(picker.filter, "acme");
        assert_eq!(picker.filtered().len(), 2);
    }

    #[test]
    fn picker_filters_navigates_and_switches_on_enter() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.overlay = Some(Overlay::Models(ModelPicker {
            models: catalog(),
            filter: String::new(),
            index: 0,
        }));

        // Typing narrows to the two acme models; Down selects the second.
        for c in "acme".chars() {
            press(&mut app, &tx, KeyCode::Char(c));
        }
        press(&mut app, &tx, KeyCode::Down);
        press(&mut app, &tx, KeyCode::Enter);

        assert!(app.overlay.is_none(), "picker closes on selection");
        match rx.try_recv() {
            Ok(WorkerCmd::SetModel { id }) => assert_eq!(id, "acme/smart-9"),
            other => panic!("expected SetModel, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn picker_escape_closes_without_switching() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.overlay = Some(Overlay::Models(ModelPicker {
            models: catalog(),
            filter: String::new(),
            index: 0,
        }));
        press(&mut app, &tx, KeyCode::Esc);
        assert!(app.overlay.is_none());
        assert!(rx.try_recv().is_err(), "no command sent on cancel");
    }

    #[test]
    fn provider_without_key_requirement_switches_directly() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.overlay = Some(Overlay::Providers {
            picker: ListPicker::new(Provider::ALL.len()),
        });
        // Down twice: openrouter -> openai -> local (needs no key).
        press(&mut app, &tx, KeyCode::Down);
        press(&mut app, &tx, KeyCode::Down);
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());
        match rx.try_recv() {
            Ok(WorkerCmd::SetProvider { provider, api_key }) => {
                assert_eq!(provider, Provider::Local);
                assert!(api_key.is_none());
            }
            other => panic!("expected SetProvider, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn api_key_prompt_masks_input_and_submits_on_enter() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.overlay = Some(Overlay::ApiKey {
            provider: Provider::OpenRouter,
            input: String::new(),
        });

        // Empty enter is ignored — no accidental keyless switch.
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_some());
        assert!(rx.try_recv().is_err());

        for c in "sk-or-abc".chars() {
            press(&mut app, &tx, KeyCode::Char(c));
        }
        // The rendered prompt shows bullets, never the key itself.
        let lines = flat_lines(&live_lines(&app, 80));
        assert!(!lines.contains("sk-or-abc"), "key must be masked: {lines}");
        assert!(lines.contains("•••••••••"));

        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());
        match rx.try_recv() {
            Ok(WorkerCmd::SetProvider { provider, api_key }) => {
                assert_eq!(provider, Provider::OpenRouter);
                assert_eq!(api_key.as_deref(), Some("sk-or-abc"));
            }
            other => panic!("expected SetProvider, got {:?}", other.is_ok()),
        }
        // The key survives to the next session via the config file.
        assert_eq!(
            crate::config::stored_key("openrouter").as_deref(),
            Some("sk-or-abc")
        );
    }

    #[test]
    fn slash_provider_opens_the_provider_overlay() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        slash_command(&mut app, "provider", &tx, 80);
        match &app.overlay {
            Some(Overlay::Providers { picker }) => assert_eq!(picker.index(), 0),
            _ => panic!("expected the provider overlay"),
        }
    }

    #[test]
    fn settings_menu_drills_into_the_matching_pickers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        slash_command(&mut app, "settings", &tx, 80);
        match &app.overlay {
            Some(Overlay::Settings { picker }) => assert_eq!(picker.index(), 0),
            _ => panic!("expected the settings overlay"),
        }

        // Provider row: opens the provider picker preselected on the
        // active provider (local sits at index 2).
        press(&mut app, &tx, KeyCode::Enter);
        match &app.overlay {
            Some(Overlay::Providers { picker }) => assert_eq!(picker.index(), 2),
            _ => panic!("expected the provider overlay"),
        }

        // Model row: kicks off the same fetch-then-pick flow as /models.
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 1),
        });
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ListModels { .. })));
        assert_eq!(app.picker_pending.as_deref(), Some(""));

        // Theme row: opens the theme picker.
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 2),
        });
        press(&mut app, &tx, KeyCode::Enter);
        assert!(matches!(app.overlay, Some(Overlay::Themes { .. })));

        // View row opens a picker preselected on the current layout.
        app.view_mode = ViewMode::Classic;
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 3),
        });
        press(&mut app, &tx, KeyCode::Enter);
        match &app.overlay {
            Some(Overlay::Views { picker }) => assert_eq!(picker.index(), 0),
            _ => panic!("expected the view overlay"),
        }

        // Down and enter selects Split using the same pattern as theme/provider.
        press(&mut app, &tx, KeyCode::Down);
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());
        assert!(app.view_mode == ViewMode::Split);
        assert_eq!(crate::config::stored_view().as_deref(), Some("split"));

        // Api key row on a keyless provider closes with an explanation.
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 4),
        });
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());
        assert!(rx.try_recv().is_err(), "no command for a keyless provider");
    }

    #[test]
    fn split_view_connects_the_selected_tool_to_its_inspector() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.view_mode = ViewMode::Split;
        let empty = rendered_rows(&mut app, 120, 24).join("\n");
        assert!(
            empty.contains("TOOL INSPECTOR"),
            "split geometry exists before calls: {empty}"
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "call-1".into(),
                tool_name: "shell".into(),
                input: serde_json::json!({
                    "command": format!("cargo test {}", "heterogeneous_burst_".repeat(6))
                }),
            },
            120,
        );

        let rail = flat_lines(&activity_lines_selected(&app, 70, true, Some(0)));
        assert!(
            rail.contains("·"),
            "selected row has a dotted leader: {rail}"
        );
        assert!(
            rail.contains('○'),
            "selected row ends at a connection node: {rail}"
        );
        let connected = activity_lines_selected(&app, 70, true, Some(0))
            .into_iter()
            .map(|line| line_text(&line))
            .find(|line| line.contains('○'))
            .expect("connector row");
        assert!(
            connected.contains("shell") && connected.chars().count() <= 70,
            "call and connector stay on one row: {connected}"
        );

        let inspector = flat_lines(&tool_inspector_lines(&app.activity_tools[0], 50));
        assert!(
            inspector.contains("SHELL") && inspector.contains("running"),
            "tool identity repeats: {inspector}"
        );
        assert!(
            inspector.contains("Run a shell command"),
            "tool action is explained: {inspector}"
        );
        assert!(
            inspector.contains("cargo test"),
            "input is expanded: {inspector}"
        );
        assert!(inspector.contains("waiting for result"));

        handle_terminal_event(
            &mut app,
            CtEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            &tx,
            120,
        );
        assert!(app.split_focused);
        press(&mut app, &tx, KeyCode::Esc);
        assert!(!app.split_focused);

        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "call-1".into(),
                tool_name: "shell".into(),
                output: serde_json::json!({
                    "stdout": (0..50).map(|line| format!("result {line}")).collect::<Vec<_>>().join("\n"),
                    "exit_code": 0
                }),
                is_error: false,
            },
            120,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::Assistant {
                message: "done".into(),
            },
            120,
        );
        assert!(app.activity_tools.is_empty(), "phase was committed");
        assert!(
            flat_lines(&app.pending_history).contains('○'),
            "the last committed call keeps its connector"
        );
        app.split_scroll = 10;
        let settled = rendered_rows(&mut app, 120, 24).join("\n");
        assert!(
            settled.contains("SHELL") && settled.contains("result"),
            "the pane stays mounted and its header stays pinned: {settled}"
        );
    }

    #[test]
    fn split_divider_runs_through_composer_and_status_rows() {
        let mut app = test_app();
        app.view_mode = ViewMode::Split;
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let inspector_x =
            Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
                .split(ratatui::layout::Rect::new(0, 0, 120, 24))[1]
                .x;
        let buffer = terminal.backend().buffer();
        for y in 0..24 {
            assert_eq!(
                buffer[(inspector_x, y)].symbol(),
                "│",
                "divider missing at row {y}"
            );
        }
    }

    #[test]
    fn capital_a_saves_the_approval_and_settings_can_revoke_it() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();

        // With nothing saved, the settings approvals row just explains.
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 5),
        });
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());

        // Capital A persists the tool for this workspace.
        let (respond, mut answer) = tokio::sync::oneshot::channel();
        app.approval = Some(crate::msg::ApprovalRequest {
            tool_name: "shell".into(),
            detail: "shell $ ls".into(),
            respond,
        });
        handle_approval_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT),
        );
        assert_eq!(
            answer.try_recv().unwrap(),
            ApprovalResponse::AllowAlwaysSave
        );
        assert_eq!(crate::config::stored_approvals("/test-ws"), ["shell"]);

        // Lowercase a stays session-only: nothing new is persisted.
        let (respond, mut answer) = tokio::sync::oneshot::channel();
        app.approval = Some(crate::msg::ApprovalRequest {
            tool_name: "write_file".into(),
            detail: "write_file x".into(),
            respond,
        });
        handle_approval_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert_eq!(answer.try_recv().unwrap(), ApprovalResponse::AllowAlways);
        assert_eq!(crate::config::stored_approvals("/test-ws"), ["shell"]);

        // The settings approvals row opens the list; enter revokes.
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 5),
        });
        press(&mut app, &tx, KeyCode::Enter);
        assert!(matches!(app.overlay, Some(Overlay::Approvals { .. })));
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none(), "removing the last entry closes");
        assert!(crate::config::stored_approvals("/test-ws").is_empty());
    }

    #[test]
    fn slash_theme_opens_a_picker_preselected_on_the_active_theme() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        slash_command(&mut app, "theme", &tx, 80);
        let current = view::theme_name();
        let expected = view::ThemeName::ALL
            .iter()
            .position(|name| *name == current)
            .unwrap();
        match &app.overlay {
            Some(Overlay::Themes { picker }) => assert_eq!(picker.index(), expected),
            other => panic!("expected theme overlay, got {}", other.is_some()),
        }

        let listing = flat_lines(&live_lines(&app, 100));
        assert!(listing.contains("Select theme"), "{listing}");
        assert!(listing.contains("Dracula"), "{listing}");
        assert!(listing.contains("current"), "{listing}");

        // Down then up returns to the active theme; enter re-applies it,
        // closes the overlay, and notes the choice. No worker involved.
        press(&mut app, &tx, KeyCode::Down);
        press(&mut app, &tx, KeyCode::Up);
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());
        assert!(rx.try_recv().is_err(), "theme switching is UI-local");
        assert_eq!(view::theme_name(), current);
        let notes = app
            .pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(notes.contains("theme set to"), "{notes}");
    }
}

#[cfg(test)]
mod theme_command_tests {
    use super::*;

    fn theme_app() -> App {
        App::new(TuiConfig {
            model_name: "m".into(),
            workspace_name: "w".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        })
    }

    /// One test, single-threaded, because the theme is process-global: it
    /// must not interleave with any other mutating test. Ends by restoring
    /// the default so later tests see a clean state.
    #[test]
    fn theme_command_switches_and_rejects() {
        let _theme = THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let mut app = theme_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        for (cmd, expected) in [
            ("theme dracula", view::ThemeName::Dracula),
            ("theme solarized-dark", view::ThemeName::SolarizedDark),
            ("theme one-dark", view::ThemeName::OneDark),
            ("theme monokai", view::ThemeName::Monokai),
            ("theme nord", view::ThemeName::Nord),
            ("theme default", view::ThemeName::Default),
            ("theme mono", view::ThemeName::Mono),
        ] {
            slash_command(&mut app, cmd, &worker, 80);
            assert_eq!(view::theme_name(), expected, "command {cmd}");
        }

        // Unknown names are rejected and leave the theme unchanged.
        slash_command(&mut app, "theme midnight", &worker, 80);
        assert_eq!(
            view::theme_name(),
            view::ThemeName::Mono,
            "unchanged on garbage"
        );

        // Bare form only reports; it must not change the value.
        slash_command(&mut app, "theme", &worker, 80);
        assert_eq!(
            view::theme_name(),
            view::ThemeName::Mono,
            "bare form does not change"
        );
        let text = app
            .pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Mono"), "reports current name: {text}");

        // Restore the default so other tests (and the view) see clean state.
        slash_command(&mut app, "theme color", &worker, 80); // legacy alias
        assert_eq!(
            view::theme_name(),
            view::ThemeName::Default,
            "color stays a legacy alias for default"
        );
    }
}

#[cfg(test)]
mod subagents_command_tests {
    use super::*;
    use orca_harness_tools::SubagentDepth;

    fn depth_app(depth: SubagentDepth) -> App {
        App::new(TuiConfig {
            model_name: "m".into(),
            workspace_name: "w".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: depth,
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        })
    }

    #[tokio::test]
    async fn subagents_command_sets_and_clamps_depth() {
        let depth = SubagentDepth::new(1);
        let mut app = depth_app(depth.clone());
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "subagents 3", &worker, 80);
        assert_eq!(depth.get(), 3);

        slash_command(&mut app, "subagents 99", &worker, 80);
        assert_eq!(depth.get(), 5, "out-of-range input clamps");

        // Bare form only reports; it must not change the value.
        slash_command(&mut app, "subagents", &worker, 80);
        assert_eq!(depth.get(), 5);

        // Garbage input leaves the value alone.
        slash_command(&mut app, "subagents lots", &worker, 80);
        assert_eq!(depth.get(), 5);
    }
}

#[cfg(test)]
mod mode_rewind_todo_tests {
    use super::*;
    use crate::mode::{Mode, ModeHandle};
    use orca_harness_tools::TodoList;

    fn app_with(mode: ModeHandle, todos: TodoList) -> App {
        App::new(TuiConfig {
            model_name: "m".into(),
            workspace_name: "w".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode,
            todos,
            plan: Default::default(),
        })
    }

    fn texts(app: &App) -> String {
        app.pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn mode_toggles_bare_and_sets_by_name() {
        let mode = ModeHandle::default();
        let mut app = app_with(mode.clone(), TodoList::new());
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "mode", &worker, 80);
        assert_eq!(mode.get(), Mode::Plan, "bare /mode toggles");
        slash_command(&mut app, "mode", &worker, 80);
        assert_eq!(mode.get(), Mode::Normal);

        slash_command(&mut app, "mode plan", &worker, 80);
        assert_eq!(mode.get(), Mode::Plan);
        // Setting the mode it is already in is not a toggle.
        slash_command(&mut app, "mode plan", &worker, 80);
        assert_eq!(mode.get(), Mode::Plan);
        slash_command(&mut app, "mode normal", &worker, 80);
        assert_eq!(mode.get(), Mode::Normal);

        // Garbage leaves the mode alone and says so.
        slash_command(&mut app, "mode sideways", &worker, 80);
        assert_eq!(mode.get(), Mode::Normal);
        // Glyphed like every other system line, not flush-left.
        assert!(texts(&app).contains("• unknown mode: sideways"));
    }

    /// Leaving plan mode reports the plans that were actually written,
    /// and stays quiet when the agent decided none was warranted —
    /// looking around in plan mode is a legitimate use of it.
    #[tokio::test]
    async fn leaving_plan_mode_reports_only_plans_that_were_written() {
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        // An episode where the agent judged no plan was needed: silence.
        let mut app = app_with(ModeHandle::new(Mode::Plan), TodoList::new());
        slash_command(&mut app, "mode normal", &worker, 80);
        let rendered = texts(&app);
        assert!(rendered.contains("normal mode"), "{rendered}");
        assert!(!rendered.contains("plan saved"), "{rendered}");
        assert!(!rendered.contains("no plan"), "{rendered}");

        // An episode where it wrote two.
        let mut app = app_with(ModeHandle::new(Mode::Plan), TodoList::new());
        app.cfg.plan.record("docs/plan/2026-08-22-first.md");
        app.cfg.plan.record("docs/plan/2026-08-22-second.md");
        slash_command(&mut app, "mode normal", &worker, 80);
        let rendered = texts(&app);
        assert!(
            rendered.contains("plan saved to docs/plan/2026-08-22-first.md"),
            "{rendered}"
        );
        assert!(
            rendered.contains("plan saved to docs/plan/2026-08-22-second.md"),
            "{rendered}"
        );
        assert!(app.cfg.plan.written().is_empty(), "the episode ended");
    }

    /// Entering plan mode must not claim anything about files — at that
    /// point nobody knows whether the conversation warrants one.
    #[tokio::test]
    async fn entering_plan_mode_says_nothing_about_files() {
        let mut app = app_with(ModeHandle::new(Mode::Normal), TodoList::new());
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        slash_command(&mut app, "mode plan", &worker, 80);
        let rendered = texts(&app);
        assert!(rendered.contains("plan mode"), "{rendered}");
        assert!(!rendered.contains("plan saved"), "{rendered}");
        assert!(!rendered.contains("docs/plan"), "{rendered}");
    }

    /// Plan mode is a restriction the user must not be able to lose
    /// track of, so it is on the status line while it is on and absent
    /// when it is not.
    #[test]
    fn plan_mode_shows_in_the_status_line() {
        let mode = ModeHandle::default();
        let plan = crate::plan::PlanArea::new();
        assert_eq!(mode_segment(&mode, &plan), "");
        mode.set(Mode::Plan);
        assert_eq!(mode_segment(&mode, &plan), " · plan mode");
        // A landed plan is visible without waiting for /mode normal.
        plan.record("docs/plan/2026-08-22-a.md");
        assert_eq!(mode_segment(&mode, &plan), " · plan mode · 1 plan");
        plan.record("docs/plan/2026-08-22-b.md");
        assert_eq!(mode_segment(&mode, &plan), " · plan mode · 2 plans");
        // Normal mode says nothing, whatever was written.
        mode.set(Mode::Normal);
        assert_eq!(mode_segment(&mode, &plan), "");
    }

    /// Write a task list through the real tool, the way the model does.
    async fn set_todos(todos: &TodoList, items: serde_json::Value) {
        let tool = orca_harness_tools::TodoWriteTool::new(todos.clone());
        let ctx = orca_harness_core::ToolContext {
            call_id: "c".into(),
            tool_name: "todo_write".into(),
            cancellation: orca_harness_core::CancellationToken::new(),
            deadline: None,
        };
        orca_harness_core::Tool::call(&tool, serde_json::json!({ "todos": items }), &ctx)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn todo_progress_shows_in_the_status_line_once_there_is_a_list() {
        let todos = TodoList::new();
        assert_eq!(todo_segment(&todos), "", "silent with no list");
        set_todos(
            &todos,
            serde_json::json!([
                {"content": "a", "status": "completed"},
                {"content": "b", "status": "in_progress"},
                {"content": "c"}
            ]),
        )
        .await;
        assert_eq!(todo_segment(&todos), " · todo 1/3");
    }

    #[tokio::test]
    async fn todo_progress_pins_the_full_plan_in_the_live_region() {
        let todos = TodoList::new();
        set_todos(
            &todos,
            serde_json::json!([
                {"content": "inspect the rendering", "status": "completed"},
                {"content": "add a visible progress cue", "status": "in_progress"},
                {"content": "verify it"}
            ]),
        )
        .await;
        let app = app_with(ModeHandle::default(), todos);

        let rendered = live_lines(&app, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("todo · 1/3 done"), "{rendered}");
        assert!(rendered.contains("├ ✓ inspect the rendering"), "{rendered}");
        assert!(
            rendered.contains("├ ▸ add a visible progress cue"),
            "{rendered}"
        );
        assert!(rendered.contains("└ □ verify it"), "{rendered}");
    }

    #[tokio::test]
    async fn completed_todo_progress_says_complete() {
        let todos = TodoList::new();
        set_todos(
            &todos,
            serde_json::json!([
                {"content": "inspect", "status": "completed"},
                {"content": "verify", "status": "completed"}
            ]),
        )
        .await;
        let app = app_with(ModeHandle::default(), todos);

        let rendered = live_lines(&app, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("todo · 2/2 done"), "{rendered}");
        assert!(rendered.contains("├ ✓ inspect"), "{rendered}");
        assert!(rendered.contains("└ ✓ verify"), "{rendered}");
    }

    #[tokio::test]
    async fn todo_renders_the_list_and_says_so_when_there_is_none() {
        let todos = TodoList::new();
        let mut app = app_with(ModeHandle::default(), todos.clone());
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "todo", &worker, 80);
        assert!(texts(&app).contains("no task list"));

        set_todos(
            &todos,
            serde_json::json!([
                {"content": "read the code", "status": "completed"},
                {"content": "write the fix", "status": "in_progress"}
            ]),
        )
        .await;

        slash_command(&mut app, "todo", &worker, 80);
        let rendered = texts(&app);
        assert!(rendered.contains("1/2 done"), "{rendered}");
        assert!(rendered.contains("✓ read the code"), "{rendered}");
        assert!(rendered.contains("▸ write the fix"), "{rendered}");
    }

    #[tokio::test]
    async fn rewind_sends_the_turn_count_and_rejects_nonsense() {
        let mut app = app_with(ModeHandle::default(), TodoList::new());
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "rewind", &worker, 80);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::Rewind { turns: 1 })));

        slash_command(&mut app, "rewind 3", &worker, 80);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::Rewind { turns: 3 })));

        // Zero and garbage send nothing and explain themselves.
        slash_command(&mut app, "rewind 0", &worker, 80);
        slash_command(&mut app, "rewind lots", &worker, 80);
        assert!(rx.try_recv().is_err(), "bad input sends no command");
        assert!(texts(&app).contains("usage: /rewind"));
    }

    #[tokio::test]
    async fn fork_asks_the_worker_to_branch() {
        let mut app = app_with(ModeHandle::default(), TodoList::new());
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();
        slash_command(&mut app, "fork", &worker, 80);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::Fork)));
    }

    /// A rewind redraws the transcript from the shortened context, but
    /// the tokens it already spent are not conversation state.
    #[test]
    fn rewind_redraws_the_transcript_and_keeps_the_token_totals() {
        let mut app = app_with(ModeHandle::default(), TodoList::new());
        let (tx, _rx) = mpsc::unbounded_channel();
        app.tokens_in = 1200;
        app.tokens_out = 340;
        app.usage_steps = 4;
        app.context_tokens = 9000;

        handle_ui_msg(
            &mut app,
            UiMsg::ContextRewound {
                messages: vec![
                    orca_harness_core::Message::System {
                        content: "sys".into(),
                    },
                    orca_harness_core::Message::User {
                        content: "still here".into(),
                    },
                ],
                notice: "rewound 1 turn · 2 messages dropped".into(),
            },
            &tx,
            80,
        );

        let rendered = texts(&app);
        assert!(rendered.contains("rewound 1 turn"), "{rendered}");
        assert!(rendered.contains("still here"), "{rendered}");
        assert_eq!(app.tokens_in, 1200, "spent tokens are not un-spent");
        assert_eq!(app.tokens_out, 340);
        assert_eq!(app.usage_steps, 4);
        assert_eq!(app.turn_count, 1, "turn count follows the new transcript");
        assert_eq!(app.context_tokens, 0, "occupancy waits for the next step");
    }

    #[test]
    fn forking_moves_the_session_id_without_touching_the_transcript() {
        let mut app = app_with(ModeHandle::default(), TodoList::new());
        let (tx, _rx) = mpsc::unbounded_channel();
        app.cfg.session_id = Some("old-id".into());
        app.turn_count = 3;

        handle_ui_msg(
            &mut app,
            UiMsg::SessionForked {
                id: "new-id".into(),
                parent: "old-id".into(),
            },
            &tx,
            80,
        );

        assert_eq!(app.cfg.session_id.as_deref(), Some("new-id"));
        assert_eq!(app.turn_count, 3, "the conversation did not change");
        let rendered = texts(&app);
        assert!(rendered.contains("forked to session new-id"), "{rendered}");
        assert!(rendered.contains("old-id is left as it was"), "{rendered}");
    }
}

#[cfg(test)]
mod extensions_command_tests {
    use super::*;

    fn ext_app() -> App {
        App::new(TuiConfig {
            model_name: "m".into(),
            workspace_name: "w".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        })
    }

    fn printed(app: &App) -> String {
        app.pending_history
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn bare_form_opens_the_picker_and_enter_toggles_in_place() {
        let mut app = ext_app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "extensions", &worker, 80);
        match &app.overlay {
            Some(Overlay::Extensions { picker }) => assert_eq!(picker.index(), 0),
            _ => panic!("expected the extensions overlay"),
        }

        // Same rendered shape as the other pickers: every extension with
        // its live state and a selection marker.
        let lines = live_lines(&app, 80)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone().into_owned()))
            .collect::<String>();
        assert!(lines.contains("truncation"), "lists truncation: {lines}");
        assert!(lines.contains("retry"), "lists retry: {lines}");
        assert!(lines.contains("enter toggle"), "shows key hint: {lines}");

        // Enter on the first row (truncation, default on) turns it off,
        // asks the worker to rebuild, and keeps the picker open.
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_overlay_key(&mut app, key, &worker);
        assert_eq!(crate::config::stored_extension("truncation"), Some(false));
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadExtensions)));
        assert!(matches!(app.overlay, Some(Overlay::Extensions { .. })));

        // A second enter toggles it right back on.
        handle_overlay_key(&mut app, key, &worker);
        assert_eq!(crate::config::stored_extension("truncation"), Some(true));
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadExtensions)));

        // Down then enter toggles the second row (retry, default off).
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        handle_overlay_key(&mut app, down, &worker);
        handle_overlay_key(&mut app, key, &worker);
        assert_eq!(crate::config::stored_extension("retry"), Some(true));

        // Esc closes like every other overlay.
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_overlay_key(&mut app, esc, &worker);
        assert!(app.overlay.is_none());
    }

    #[tokio::test]
    async fn typed_form_saves_the_toggle_and_reloads() {
        let mut app = ext_app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "extensions enable retry", &worker, 80);
        assert_eq!(crate::config::stored_extension("retry"), Some(true));
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadExtensions)));
        assert!(printed(&app).contains("extension retry enabled"));

        slash_command(&mut app, "extensions disable truncation", &worker, 80);
        assert_eq!(crate::config::stored_extension("truncation"), Some(false));
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadExtensions)));

        // "add" and "delete" are accepted aliases.
        slash_command(&mut app, "extensions delete retry", &worker, 80);
        assert_eq!(crate::config::stored_extension("retry"), Some(false));
        slash_command(&mut app, "extensions add truncation", &worker, 80);
        assert_eq!(crate::config::stored_extension("truncation"), Some(true));
    }

    #[tokio::test]
    async fn bad_input_reports_and_sends_nothing() {
        let mut app = ext_app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "extensions enable nope", &worker, 80);
        assert!(printed(&app).contains("unknown extension: nope"));
        assert!(
            printed(&app).contains("truncation, retry"),
            "names the valid set: {}",
            printed(&app)
        );

        slash_command(&mut app, "extensions frobnicate retry", &worker, 80);
        assert!(printed(&app).contains("usage: /extensions"));

        assert!(rx.try_recv().is_err(), "bad input sends nothing");
    }

    fn session_file(id: &str, model: &str) -> orca_harness_extensions::SessionFile {
        orca_harness_extensions::SessionFile {
            path: std::path::PathBuf::from(format!("/tmp/{id}.jsonl")),
            meta: orca_harness_extensions::SessionMeta {
                v: orca_harness_extensions::SESSION_FORMAT_VERSION,
                id: id.into(),
                created_at: 0,
                workspace: "/test-ws".into(),
                model: model.into(),
                parent: None,
            },
        }
    }

    #[tokio::test]
    async fn sessions_picker_navigates_and_enter_resumes() {
        let mut app = ext_app();
        app.cfg.session_id = Some("0000000002-b-0".into());
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // Newest first, preselected on the current session (row 0).
        app.overlay = Some(Overlay::Sessions {
            sessions: vec![
                session_file("0000000002-b-0", "m2"),
                session_file("0000000001-a-0", "m1"),
            ],
            picker: ListPicker::new(2),
        });

        // Same rendered shape as the other pickers: every session with a
        // selection marker, the current one labeled.
        let lines = live_lines(&app, 100)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone().into_owned()))
            .collect::<String>();
        assert!(lines.contains("enter resume"), "shows key hint: {lines}");
        assert!(lines.contains("(current)"), "marks current: {lines}");
        assert!(lines.contains("0000000001-a-0"), "lists both: {lines}");

        // Down then enter resumes the older session and closes the picker.
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_overlay_key(&mut app, down, &worker);
        handle_overlay_key(&mut app, enter, &worker);
        match rx.try_recv() {
            Ok(WorkerCmd::LoadSession { path }) => {
                assert_eq!(path, std::path::PathBuf::from("/tmp/0000000001-a-0.jsonl"));
            }
            other => panic!("expected LoadSession, got {:?}", other.is_ok()),
        }
        assert!(app.overlay.is_none());

        // Esc closes like every other overlay.
        app.overlay = Some(Overlay::Sessions {
            sessions: vec![session_file("0000000001-a-0", "m1")],
            picker: ListPicker::new(1),
        });
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_overlay_key(&mut app, esc, &worker);
        assert!(app.overlay.is_none());
    }

    #[tokio::test]
    async fn space_d_deletes_a_session_but_never_the_active_one() {
        let dir = std::env::temp_dir().join(format!("orca-tui-del-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let on_disk = |id: &str, model: &str| {
            let mut session = session_file(id, model);
            session.path = dir.join(format!("{id}.jsonl"));
            std::fs::write(&session.path, "{}\n").unwrap();
            session
        };

        let mut app = ext_app();
        app.cfg.session_id = Some("0000000002-b-0".into());
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let active = on_disk("0000000002-b-0", "m2");
        let old = on_disk("0000000001-a-0", "m1");
        let active_path = active.path.clone();
        let old_path = old.path.clone();
        app.overlay = Some(Overlay::Sessions {
            sessions: vec![active, old],
            picker: ListPicker::new(2).actions(SESSION_ACTIONS),
        });

        // Space + d on the active session (row 0): refused, file kept.
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        let d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE);
        handle_overlay_key(&mut app, space, &worker);
        handle_overlay_key(&mut app, d, &worker);
        assert!(active_path.exists(), "active session file kept");
        assert!(printed(&app).contains("cannot be deleted"));
        assert!(matches!(app.overlay, Some(Overlay::Sessions { .. })));

        // Down, space + d: the old session is deleted and the list
        // shrinks in place with the picker still open.
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        handle_overlay_key(&mut app, down, &worker);
        handle_overlay_key(&mut app, space, &worker);
        handle_overlay_key(&mut app, d, &worker);
        assert!(!old_path.exists(), "old session file removed");
        assert!(printed(&app).contains("deleted session 0000000001-a-0"));
        match &app.overlay {
            Some(Overlay::Sessions { sessions, picker }) => {
                assert_eq!(sessions.len(), 1);
                assert_eq!(picker.index(), 0);
            }
            _ => panic!("picker stays open while rows remain"),
        }
        assert!(rx.try_recv().is_err(), "deleting sends nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sessions_picker_windows_to_the_last_few_and_pages_like_models() {
        let mut app = ext_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        // 12 recorded sessions, newest first; the current one is not in
        // the newest five, so the picker opens on an older row that the
        // window would otherwise hide.
        let sessions: Vec<_> = (0..12)
            .map(|n| session_file(&format!("00000000{n:02}-m{n}-0"), &format!("m{n}")))
            .collect();
        app.cfg.session_id = Some("0000000005-m5-0".into());
        app.overlay = Some(Overlay::Sessions {
            sessions,
            picker: ListPicker::with_selected(12, 5).actions(SESSION_ACTIONS),
        });

        // The window shows a bounded slice of the list with the
        // position in the header — the /models display grammar — and
        // the older sessions are still reachable by paging.
        let lines = live_lines(&app, 260)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone().into_owned()))
            .collect::<String>();
        assert!(lines.contains("6/12"), "position shown: {lines}");
        assert!(lines.contains("enter resume"), "key hints: {lines}");
        assert!(lines.contains("(current)"), "marks current: {lines}");

        // Page down past the window into the older rows: the cursor
        // moves without closing the picker.
        let pgdn = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        handle_overlay_key(&mut app, pgdn, &worker);
        match &app.overlay {
            Some(Overlay::Sessions { picker, .. }) => assert_eq!(picker.index(), 11),
            _ => panic!("sessions overlay stays open"),
        }
    }

    #[tokio::test]
    async fn session_loaded_replays_the_transcript() {
        let mut app = ext_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        let call = orca_harness_core::ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({"command": "ls"}),
        };
        let messages = vec![
            orca_harness_core::Message::System {
                content: "sys".into(),
            },
            orca_harness_core::Message::User {
                content: "first prompt".into(),
            },
            orca_harness_core::Message::Assistant {
                content: None,
                tool_calls: vec![call.clone()],
            },
            orca_harness_core::Message::Tool {
                results: vec![orca_harness_core::ToolResult::ok(
                    &call,
                    serde_json::json!({"stdout": "a\n", "success": true}),
                )],
            },
            orca_harness_core::Message::Assistant {
                content: Some("the answer".into()),
                tool_calls: vec![],
            },
        ];
        handle_ui_msg(
            &mut app,
            UiMsg::SessionLoaded {
                id: "s1".into(),
                messages,
            },
            &worker,
            80,
        );

        assert_eq!(app.cfg.session_id.as_deref(), Some("s1"));
        let text = printed(&app);
        assert!(text.contains("resumed session s1 (5 messages)"), "{text}");
        assert!(text.contains("┃ first prompt"), "spine replayed: {text}");
        assert!(text.contains("shell"), "tool call replayed: {text}");
        assert!(text.contains("the answer"), "answer replayed: {text}");
    }
}

#[cfg(test)]
mod mcp_command_tests {
    use super::*;

    fn mcp_app() -> App {
        App::new(TuiConfig {
            model_name: "m".into(),
            workspace_name: "w".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        })
    }

    fn printed(app: &App) -> String {
        app.pending_history
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(app: &mut App, tx: &mpsc::UnboundedSender<WorkerCmd>, code: KeyCode) {
        handle_terminal_event(
            app,
            CtEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            tx,
            80,
        );
    }

    /// The overlay as drawn, one string per line.
    fn overlay_text(app: &App) -> String {
        live_lines(app, 120)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn add_list_and_remove_round_trip_through_config_and_reload() {
        let mut app = mcp_app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // Bare form with nothing configured points at the add syntax
        // rather than opening an overlay with no rows to toggle.
        slash_command(&mut app, "mcp", &worker, 80);
        assert!(printed(&app).contains("no MCP servers configured"));
        assert!(app.overlay.is_none());

        // Add saves the command verbatim (arguments included) and asks
        // the worker to reconnect.
        slash_command(
            &mut app,
            "mcp add docs npx -y some-server /tmp",
            &worker,
            80,
        );
        let stored = crate::config::stored_mcp_servers();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].name, "docs");
        assert_eq!(stored[0].command, "npx -y some-server /tmp");
        assert!(stored[0].enabled, "a new server starts on");
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadMcp)));
        assert!(printed(&app).contains("mcp server docs added"));

        // Bare form now opens the picker, listing state and command.
        // The count is "…" until a reload reports one.
        slash_command(&mut app, "mcp", &worker, 80);
        assert!(matches!(app.overlay, Some(Overlay::Mcp { .. })));
        let text = overlay_text(&app);
        assert!(text.contains("space toggle"), "{text}");
        assert!(text.contains("docs  on   …"), "{text}");
        assert!(text.contains("npx -y some-server /tmp"), "{text}");
        press(&mut app, &worker, KeyCode::Esc);

        // Remove drops it and reconnects; "rm" and "delete" are aliases.
        slash_command(&mut app, "mcp remove docs", &worker, 80);
        assert!(crate::config::stored_mcp_servers().is_empty());
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadMcp)));
        assert!(printed(&app).contains("mcp server docs removed"));
    }

    /// Space toggles the selected row: the config is written, the
    /// worker is asked to reconnect, and the overlay stays open showing
    /// the new state at once.
    #[tokio::test]
    async fn space_toggles_the_selected_server_and_the_overlay_stays_open() {
        let mut app = mcp_app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();
        crate::config::save_mcp_server("docs", "run docs").unwrap();
        crate::config::save_mcp_server("fetch", "run fetch").unwrap();

        slash_command(&mut app, "mcp", &worker, 80);
        // Config order is the map's: docs, then fetch.
        press(&mut app, &worker, KeyCode::Down);
        press(&mut app, &worker, KeyCode::Char(' '));

        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadMcp)));
        assert!(
            matches!(app.overlay, Some(Overlay::Mcp { .. })),
            "toggling keeps the overlay open for the next row"
        );
        let stored = crate::config::stored_mcp_servers();
        assert!(stored[0].enabled, "the unselected row is untouched");
        assert!(!stored[1].enabled, "fetch is now off");

        // The row redraws immediately, without waiting for the reload,
        // and an off server shows no tool count.
        let text = overlay_text(&app);
        assert!(text.contains("docs   on   …"), "{text}");
        assert!(text.contains("fetch  off"), "{text}");
        assert!(!text.contains("fetch  off  …"), "off rows drop the count");

        // Enter toggles too, matching /extensions muscle memory.
        press(&mut app, &worker, KeyCode::Enter);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadMcp)));
        assert!(crate::config::stored_mcp_servers()[1].enabled);
        assert!(overlay_text(&app).contains("fetch  on"));

        press(&mut app, &worker, KeyCode::Esc);
        assert!(app.overlay.is_none());
    }

    #[test]
    fn mcp_picker_filters_by_typing_and_reports_position() {
        let mut app = mcp_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        crate::config::save_mcp_server("alpha", "run alpha").unwrap();
        crate::config::save_mcp_server("beta", "run beta").unwrap();

        slash_command(&mut app, "mcp", &worker, 80);
        assert!(overlay_text(&app).contains("1/2"));
        press(&mut app, &worker, KeyCode::Char('b'));
        let text = overlay_text(&app);
        assert!(text.contains("filter: b"), "{text}");
        assert!(text.contains("beta"), "{text}");
        assert!(!text.contains("run alpha"), "{text}");
        assert!(text.contains("1/1"), "{text}");

        press(&mut app, &worker, KeyCode::Backspace);
        assert!(overlay_text(&app).contains("1/2"));
        crate::config::remove_mcp_server("alpha").unwrap();
        crate::config::remove_mcp_server("beta").unwrap();
    }

    /// Tool counts and connection errors come off the shared handle the
    /// worker reloads. The reason goes after the command so a long error
    /// never truncates the row before the command is visible.
    #[tokio::test]
    async fn rows_report_tool_counts_and_connection_errors() {
        let mut app = mcp_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        crate::config::save_mcp_server("ghost", "orca-no-such-binary-xyz").unwrap();
        app.cfg.mcp.reload().await;

        slash_command(&mut app, "mcp", &worker, 80);
        let text = overlay_text(&app);
        assert!(text.contains("ghost  on   failed"), "{text}");
        assert!(
            text.contains("failed     orca-no-such-binary-xyz"),
            "the command survives the error: {text}"
        );
        assert!(text.contains("spawn failed"), "the reason is shown: {text}");
    }

    /// Env references survive redaction — they name a variable, they do
    /// not carry it — so the row still says which one a server needs.
    #[test]
    fn redaction_keeps_env_references_and_the_rest_of_the_command() {
        let command = "npx -y mcp-remote https://api.githubcopilot.com/mcp/readonly \
                       --header Authorization:${AUTH_HEADER}";
        assert_eq!(redact_command(command), command);
    }

    #[test]
    fn redaction_masks_literal_credentials_in_every_shape() {
        // The shape a user actually produces: `--header` values cannot
        // contain spaces (the command is whitespace-split), so a pasted
        // credential arrives glued to the header name. The name survives
        // so the row still says what is being sent.
        assert_eq!(
            redact_command("npx mcp-remote https://x.dev/mcp --header Authorization:ghp_realtoken"),
            "npx mcp-remote https://x.dev/mcp --header Authorization:<redacted>"
        );
        // Being an Authorization value is enough on its own — the value
        // need not look token-shaped.
        assert_eq!(
            redact_command("x --header Authorization:Bearer"),
            "x --header Authorization:<redacted>"
        );
        // An unrecognized header name masks the whole word rather than
        // guessing which half is the secret; losing the name is the safe
        // direction.
        assert_eq!(
            redact_command("x --header X-Custom-Auth:ghp_realtoken"),
            "x --header <redacted>"
        );
        // Flag and value in one word.
        assert_eq!(
            redact_command("some-server --api-key=sk-abc123"),
            "some-server --api-key=<redacted>"
        );
        // Flag and value split across words.
        assert_eq!(
            redact_command("some-server --token sk-abc123"),
            "some-server --token <redacted>"
        );
        // A bare token as a positional argument.
        assert_eq!(
            redact_command("some-server github_pat_11ABCDE"),
            "some-server <redacted>"
        );
        // Credentials inside the URL: query parameter and userinfo.
        assert_eq!(
            redact_command("npx mcp-remote https://x.dev/sse?api_key=abc123&mode=fast"),
            "npx mcp-remote https://x.dev/sse?api_key=<redacted>&mode=fast"
        );
        assert_eq!(
            redact_command("npx mcp-remote https://user:hunter2@x.dev/mcp"),
            "npx mcp-remote https://user:<redacted>@x.dev/mcp"
        );
    }

    /// Redaction must not chew up ordinary commands: no flags, no
    /// tokens, nothing that merely looks like one.
    #[test]
    fn redaction_leaves_ordinary_commands_alone() {
        for command in [
            "uvx mcp-server-fetch",
            "npx -y @modelcontextprotocol/server-everything",
            "npx -y mcp-remote https://mcp.context7.com/mcp",
            "npx -y mcp-remote https://gitmcp.io/okikorg/orca",
            "github-mcp-server stdio --toolsets repos,issues",
        ] {
            assert_eq!(redact_command(command), command, "mangled: {command}");
        }
    }

    /// The overlay renders the redacted form, never the stored one.
    #[tokio::test]
    async fn the_picker_never_renders_a_literal_token() {
        let mut app = mcp_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        crate::config::save_mcp_server(
            "github",
            "npx -y mcp-remote https://x.dev/mcp --header Authorization:ghp_supersecret",
        )
        .unwrap();

        slash_command(&mut app, "mcp", &worker, 80);
        let text = overlay_text(&app);
        assert!(!text.contains("ghp_supersecret"), "token on screen: {text}");
        assert!(text.contains("Authorization:<redacted>"), "{text}");
        // The config keeps the real value — this is display-only.
        assert!(crate::config::stored_mcp_servers()[0]
            .command
            .contains("ghp_supersecret"));
    }

    /// The sequence the overlay exists for: toggle on, the row shows `…`
    /// while the reconnect runs, and the count lands once it reports.
    #[tokio::test]
    async fn a_toggled_on_server_moves_from_the_placeholder_to_its_state() {
        let mut app = mcp_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        crate::config::save_mcp_server("ghost", "orca-no-such-binary-xyz").unwrap();
        crate::config::set_mcp_enabled("ghost", false).unwrap();
        app.cfg.mcp.reload().await;

        slash_command(&mut app, "mcp", &worker, 80);
        assert!(overlay_text(&app).contains("ghost  off"));

        // Toggling on redraws as on with no state yet; the worker has
        // not reconnected.
        press(&mut app, &worker, KeyCode::Char(' '));
        assert!(overlay_text(&app).contains("ghost  on   …"));

        // The worker's reload resolves it, with the overlay still open.
        app.cfg.mcp.reload().await;
        let text = overlay_text(&app);
        assert!(!text.contains('…'), "the placeholder resolves: {text}");
        assert!(text.contains("ghost  on   failed"), "{text}");
    }

    #[tokio::test]
    async fn bad_input_reports_and_sends_nothing() {
        let mut app = mcp_app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // add needs both a name and a command.
        slash_command(&mut app, "mcp add", &worker, 80);
        slash_command(&mut app, "mcp add docs", &worker, 80);
        assert!(printed(&app).contains("usage: /mcp"));

        // Names feed the model-facing tool prefix, so junk is rejected.
        slash_command(&mut app, "mcp add bad/name run it", &worker, 80);
        assert!(printed(&app).contains("invalid server name: bad/name"));

        // Removing something that was never added names the valid set.
        slash_command(&mut app, "mcp remove nope", &worker, 80);
        assert!(printed(&app).contains("unknown mcp server: nope"));

        // Re-adding a server the user turned off edits it without
        // enabling it, and says so rather than claiming to connect.
        crate::config::save_mcp_server("docs", "run docs").unwrap();
        crate::config::set_mcp_enabled("docs", false).unwrap();
        slash_command(&mut app, "mcp add docs run other", &worker, 80);
        let stored = crate::config::stored_mcp_servers();
        assert_eq!(stored[0].command, "run other");
        assert!(!stored[0].enabled, "an edit is not an enable");
        assert!(printed(&app).contains("mcp server docs updated — still off"));
        crate::config::remove_mcp_server("docs").unwrap();
        while rx.try_recv().is_ok() {}

        slash_command(&mut app, "mcp frobnicate", &worker, 80);
        assert!(printed(&app).contains("usage: /mcp"));

        assert!(crate::config::stored_mcp_servers().is_empty());
        assert!(rx.try_recv().is_err(), "bad input sends nothing");
    }
}

#[cfg(test)]
mod stats_segment_tests {
    use super::*;

    #[test]
    fn segments_render_only_nonzero_counts() {
        let stats = orca_harness_tools::BackgroundStats::new();
        assert_eq!(stats_segments(&stats), "");
        stats.inc_processes();
        stats.inc_processes();
        stats.inc_agents();
        assert_eq!(stats_segments(&stats), " · procs 2 · agents 1");
        stats.inc_kernels();
        assert_eq!(stats_segments(&stats), " · procs 2 · pykernel · agents 1");
    }
}

#[cfg(test)]
mod nested_rail_tests {
    use super::*;
    use orca_harness_extensions::HarnessEvent;
    use serde_json::json;

    fn nested_app() -> App {
        App::new(TuiConfig {
            model_name: "m".into(),
            workspace_name: "w".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        })
    }

    fn rail_text(app: &App) -> String {
        activity_lines(app, 120, true)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn inner_tools_render_indented_under_the_subagent_line() {
        let mut app = nested_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "explore"}),
            },
            120,
        );
        handle_subagent_event(
            &mut app,
            7,
            None,
            0,
            "c1".into(),
            HarnessEvent::ToolCall {
                tool_call_id: "i1".into(),
                tool_name: "list_dir".into(),
                input: json!({"path": "."}),
            },
        );
        let text = rail_text(&app);
        assert!(text.contains("subagent"), "rail: {text}");
        assert!(text.contains("list_dir"), "rail: {text}");
        let inner_line = text.lines().find(|l| l.contains("list_dir")).unwrap();
        assert!(
            inner_line.starts_with("      "),
            "inner line must be indented: {inner_line:?}"
        );
        assert!(
            inner_line.contains("└─") || inner_line.contains("├─"),
            "inner line must carry a tree branch so ownership is unambiguous: {inner_line:?}"
        );
        let outer_line = text.lines().find(|l| l.contains("subagent")).unwrap();
        let branch_col = |l: &str| l.find(['└', '├']).unwrap();
        assert!(
            branch_col(inner_line) > branch_col(outer_line),
            "inner branch must sit deeper than the subagent's own branch:\n{outer_line}\n{inner_line}"
        );

        handle_subagent_event(
            &mut app,
            7,
            None,
            0,
            "c1".into(),
            HarnessEvent::ToolResult {
                tool_call_id: "i1".into(),
                tool_name: "list_dir".into(),
                output: json!({"entries": []}),
                is_error: false,
            },
        );
        let text = rail_text(&app);
        let inner_line = text.lines().find(|l| l.contains("list_dir")).unwrap();
        assert!(inner_line.contains("✓"), "completed glyph: {inner_line:?}");
    }

    #[test]
    fn deeper_spawns_indent_further() {
        let mut app = nested_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "outer"}),
            },
            120,
        );
        handle_subagent_event(
            &mut app,
            1,
            None,
            0,
            "c1".into(),
            HarnessEvent::ToolCall {
                tool_call_id: "i1".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "inner"}),
            },
        );
        handle_subagent_event(
            &mut app,
            2,
            Some(1),
            1,
            "i1".into(),
            HarnessEvent::ToolCall {
                tool_call_id: "g1".into(),
                tool_name: "grep".into(),
                input: json!({"pattern": "x"}),
            },
        );
        let text = rail_text(&app);
        let child = text
            .lines()
            .find(|l| l.contains("subagent {\"task\":\"inner"))
            .unwrap();
        let grandchild = text.lines().find(|l| l.contains("grep")).unwrap();
        let indent = |l: &str| l.chars().take_while(|c| *c == ' ').count();
        assert!(
            indent(grandchild) > indent(child),
            "child: {child:?} grandchild: {grandchild:?}"
        );
    }

    #[test]
    fn completion_folds_inner_log_into_the_expandable_record() {
        let mut app = nested_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "explore"}),
            },
            120,
        );
        handle_subagent_event(
            &mut app,
            7,
            None,
            0,
            "c1".into(),
            HarnessEvent::ToolCall {
                tool_call_id: "i1".into(),
                tool_name: "list_dir".into(),
                input: json!({"path": "."}),
            },
        );
        handle_subagent_event(
            &mut app,
            7,
            None,
            0,
            "c1".into(),
            HarnessEvent::ToolResult {
                tool_call_id: "i1".into(),
                tool_name: "list_dir".into(),
                output: json!({"entries": []}),
                is_error: false,
            },
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                output: json!({"answer": "found things"}),
                is_error: false,
            },
            120,
        );

        assert!(
            app.subagent_activity.is_empty(),
            "spawn state must fold away"
        );
        let record = app.tool_log.last().unwrap();
        assert!(record.inner.iter().any(|l| l.contains("list_dir")));

        expand_tool(&mut app, 1, 120);
        let expanded: String = app
            .pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(expanded.contains("inner activity"), "{expanded}");
        assert!(expanded.contains("list_dir"), "{expanded}");
    }
}

#[cfg(test)]
mod skills_command_tests {
    use super::*;

    /// A temp tree plus the `Skills` handle that scans it. Roots are
    /// passed in explicitly, so a test never reaches the developer's own
    /// ~/.claude/skills.
    struct Fixture {
        dir: std::path::PathBuf,
        skills: crate::skills::Skills,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "orca-tui-skills-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let skills = crate::skills::Skills::new(&dir, None, None);
            Self { dir, skills }
        }

        fn skill(&self, name: &str, description: &str) {
            let path = self.dir.join(".orca/skills").join(name).join("SKILL.md");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                format!("---\nname: {name}\ndescription: {description}\n---\n\nstep one\n"),
            )
            .unwrap();
            self.skills.reload();
        }

        fn app(&self) -> App {
            App::new(TuiConfig {
                model_name: "m".into(),
                workspace_name: "w".into(),
                workspace_root: "/test-ws".into(),
                provider: Provider::Local,
                subagent_depth: orca_harness_tools::SubagentDepth::new(1),
                stats: orca_harness_tools::BackgroundStats::new(),
                session_id: None,
                mcp: Default::default(),
                skills: self.skills.clone(),
                mode: Default::default(),
                todos: Default::default(),
                plan: Default::default(),
            })
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn printed(app: &App) -> String {
        app.pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn overlay_text(app: &App) -> String {
        live_lines(app, 120)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Nothing found: the bare form says how to get one instead of
    /// opening an overlay with no rows in it.
    #[test]
    fn empty_catalog_points_at_add_and_create() {
        let fixture = Fixture::new("empty");
        let mut app = fixture.app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills", &worker, 80);
        assert!(app.overlay.is_none());
        let text = printed(&app);
        assert!(text.contains("/skills add"), "{text}");
        assert!(text.contains("/skills create"), "{text}");
        assert!(text.contains(".claude/skills"), "{text}");
    }

    fn press(app: &mut App, worker: &mpsc::UnboundedSender<WorkerCmd>, code: KeyCode) {
        handle_terminal_event(
            app,
            CtEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            worker,
            80,
        );
    }

    /// Space reveals the strip rather than acting, so neither toggling
    /// nor deleting is one stray keystroke away.
    #[test]
    fn space_reveals_the_actions_and_t_toggles() {
        let fixture = Fixture::new("toggle");
        fixture.skill("release", "Cut a release");
        let mut app = fixture.app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills", &worker, 80);
        let shown = overlay_text(&app);
        assert!(shown.contains("release"), "{shown}");
        assert!(shown.contains("Cut a release"), "{shown}");
        assert!(shown.contains("space actions"), "{shown}");

        press(&mut app, &worker, KeyCode::Char(' '));
        let armed = overlay_text(&app);
        assert!(armed.contains("[t] toggle"), "{armed}");
        assert!(armed.contains("[d] delete"), "{armed}");
        assert_eq!(
            crate::config::stored_skill_enabled("release"),
            None,
            "space itself changes nothing"
        );

        press(&mut app, &worker, KeyCode::Char('t'));
        assert_eq!(
            crate::config::stored_skill_enabled("release"),
            Some(false),
            "the action key writes the override"
        );
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadSkills)));
        // The overlay stays open and the row redraws off at once, while
        // the rescan and rebuild run behind it.
        assert!(app.overlay.is_some());
        assert!(overlay_text(&app).contains("off"), "{}", overlay_text(&app));

        // Enter keeps the one-key path for the common case.
        press(&mut app, &worker, KeyCode::Enter);
        assert_eq!(crate::config::stored_skill_enabled("release"), Some(true));
    }

    #[test]
    fn skills_picker_filters_by_typing_and_backspace() {
        let fixture = Fixture::new("filter");
        fixture.skill("deploy", "Ship the application");
        fixture.skill("review", "Review a change");
        let mut app = fixture.app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills", &worker, 80);
        assert!(overlay_text(&app).contains("1/2"));
        press(&mut app, &worker, KeyCode::Char('r'));
        let text = overlay_text(&app);
        assert!(text.contains("filter: r"), "{text}");
        assert!(text.contains("review"), "{text}");
        assert!(!text.contains("deploy"), "{text}");
        assert!(text.contains("1/1"), "{text}");

        press(&mut app, &worker, KeyCode::Backspace);
        assert!(overlay_text(&app).contains("1/2"));
    }

    /// Delete removes the folder, the row, and the saved override — and
    /// only for skills this host installed.
    #[test]
    fn delete_action_removes_an_installed_skill() {
        let fixture = Fixture::new("delete");
        fixture.skill("release", "Cut a release");
        let mut app = fixture.app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let dir = fixture.dir.join(".orca/skills/release");
        assert!(dir.is_dir());

        slash_command(&mut app, "skills", &worker, 80);
        press(&mut app, &worker, KeyCode::Char(' '));
        press(&mut app, &worker, KeyCode::Char('d'));

        assert!(!dir.exists(), "the folder is gone");
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadSkills)));
        // Last row deleted: the overlay closes rather than showing an
        // empty list.
        assert!(app.overlay.is_none());
        assert!(
            printed(&app).contains("removed release"),
            "{}",
            printed(&app)
        );
    }

    /// The whole loop against the real internet: install a published
    /// skill from GitHub through `/skills add`, confirm the running
    /// agent is offered it, then delete it from the overlay. Ignored by
    /// default — it clones a repository and spawns the built binary
    /// against a local model endpoint.
    ///
    /// Run with:
    /// `cargo test -p orcacode --bin orcacode e2e_ -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "network: clones a github repository and calls a model"]
    async fn e2e_add_use_and_remove_a_published_skill() {
        let fixture = Fixture::new("e2e");
        let config = fixture.dir.join("config");
        let workspace = fixture.dir.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("config.json"),
            r#"{"provider": "local", "models": {"local": "gemma4:e2b-mlx"}}"#,
        )
        .unwrap();
        let skills = crate::skills::Skills::new(&workspace, Some(config.clone()), None);
        let mut app = App::new(TuiConfig {
            model_name: "e2e".into(),
            workspace_name: "ws".into(),
            workspace_root: workspace.display().to_string(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: skills.clone(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        });
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // 1. Add, exactly as the composer would.
        slash_command(
            &mut app,
            "skills add vercel-labs/agent-skills --skill writing-guidelines",
            &worker,
            80,
        );
        let Ok(WorkerCmd::InstallSkill { source, here }) = rx.try_recv() else {
            panic!("no install command: {}", printed(&app));
        };
        assert!(!here, "installs beside config.json by default");
        // What the worker does with it.
        let lines = skills.add(&source, here).await.expect("install");
        println!("{}", lines.join("\n"));
        skills.reload();
        let installed = config.join("skills/writing-guidelines/SKILL.md");
        assert!(installed.is_file(), "SKILL.md landed at {installed:?}");

        // 2. The running agent is offered it, by name, with its blurb.
        let tool = skills.tool().expect("a skill tool");
        let schema = tool.schema();
        assert_eq!(schema.name, "skill");
        assert!(
            schema.description.contains("writing-guidelines"),
            "{}",
            schema.description
        );

        // 3. A real model, given the real binary, calls it. The tool log
        //    goes to stderr, so that is where the call shows up.
        // The test binary lives in target/<profile>/deps/, so the CLI
        // it was built alongside is two directories up.
        let test_binary = std::env::current_exe().expect("test binary path");
        let binary = test_binary
            .parent()
            .and_then(std::path::Path::parent)
            .expect("target dir")
            .join("orcacode");
        assert!(binary.is_file(), "build the binary first: {binary:?}");
        let run = std::process::Command::new(&binary)
            .env("ORCA_CONFIG_DIR", &config)
            .args(["--workspace"])
            .arg(&workspace)
            .args([
                "--no-session",
                "--auto-approve",
                "--max-steps",
                "4",
                "-p",
                "Load the writing-guidelines skill and quote its first heading. \
                 Use the skill tool.",
            ])
            .output()
            .expect("run orcacode");
        let stderr = String::from_utf8_lossy(&run.stderr);
        let stdout = String::from_utf8_lossy(&run.stdout);
        println!("--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");
        assert!(
            stderr.contains("skill"),
            "the model never reached for the skill tool"
        );

        // 4. Remove it from the overlay: space reveals, d deletes.
        slash_command(&mut app, "skills", &worker, 80);
        let shown = overlay_text(&app);
        assert!(shown.contains("writing-guidelines"), "{shown}");
        press(&mut app, &worker, KeyCode::Char(' '));
        press(&mut app, &worker, KeyCode::Char('d'));
        assert!(
            !config.join("skills/writing-guidelines").exists(),
            "the folder is gone"
        );
        skills.reload();
        assert!(skills.tool().is_none(), "and so is the tool");
    }

    /// A skill from a compatibility root is not this host's to delete.
    #[test]
    fn delete_refuses_a_skill_from_a_root_it_does_not_own() {
        let fixture = Fixture::new("foreign");
        let path = fixture.dir.join(".claude/skills/borrowed/SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nname: borrowed\ndescription: someone else's\n---\n\nbody\n",
        )
        .unwrap();
        fixture.skills.reload();
        let mut app = fixture.app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills remove borrowed", &worker, 80);
        assert!(path.is_file(), "the file must survive");
        let text = printed(&app);
        assert!(text.contains("only deletes what it installed"), "{text}");
    }

    #[test]
    fn show_reports_one_skill_and_rejects_unknown_names() {
        let fixture = Fixture::new("show");
        fixture.skill("release", "Cut a release");
        let mut app = fixture.app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills show release", &worker, 80);
        let text = printed(&app);
        assert!(text.contains(".orca/skills"), "{text}");
        assert!(text.contains("Cut a release"), "{text}");

        slash_command(&mut app, "skills show nope", &worker, 80);
        let text = printed(&app);
        assert!(text.contains("unknown skill: nope"), "{text}");
        assert!(text.contains("found: release"), "{text}");
    }

    #[test]
    fn reload_goes_through_the_worker_and_garbage_is_rejected() {
        let fixture = Fixture::new("reload");
        let mut app = fixture.app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills reload", &worker, 80);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadSkills)));
        assert!(printed(&app).contains("rescanning skills"));

        slash_command(&mut app, "skills wat", &worker, 80);
        assert!(rx.try_recv().is_err(), "no command for a bad argument");
        assert!(printed(&app).contains("unknown /skills argument: wat"));
    }
}
