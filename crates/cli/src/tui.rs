//! The interactive terminal. Fullscreen alternate-screen app: the
//! transcript fills the window from the top, a live region (streaming
//! tail, approval prompts, the slash palette) sits above the composer,
//! and the composer plus status line are pinned to the bottom. PgUp/PgDn
//! scroll the in-app transcript buffer.

use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream as CtEventStream,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;

use orca_harness_core::CancellationToken;
use orca_harness_extensions::HarnessEvent;
use orca_harness_model_openrouter::ModelInfo;

use crate::commands::{filter_commands, CommandSpec};
use crate::components::picker::{ListPicker, PickerAction, PickerEvent};
use crate::msg::{ApprovalRequest, ApprovalResponse, Provider, UiMsg, WorkerCmd};
use crate::view::{self, theme};

const SPINNER: &[char] = &['·', ' '];
const EXPAND_MAX_LINES: usize = 200;
const TRANSCRIPT_CAP: usize = 5000;
const INSPECTOR_PREVIEW_LINES: usize = 240;
const INSPECTOR_PREVIEW_CHARS: usize = 32 * 1024;
const INSPECTOR_OUTPUT_HEAD: usize = 16;
const INSPECTOR_OUTPUT_TAIL: usize = 6;
const SCROLL_PAGE: usize = 10;
const PALETTE_ROWS: usize = 8;
const PICKER_ROWS: usize = 10;
const LIVE_TOOL_ROWS: usize = 8;
const QUEUE_PREVIEW_ROWS: usize = 3;
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
}

/// The interactive model selector: the fetched catalog, a live-typed
/// filter, and the selected row (an index into the filtered view).
struct ModelPicker {
    models: Vec<ModelInfo>,
    filter: String,
    index: usize,
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
    prompt_queue: VecDeque<String>,
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
}

impl App {
    fn new(cfg: TuiConfig) -> Self {
        Self {
            cfg,
            pending_history: Vec::new(),
            transcript: Vec::new(),
            scroll: 0,
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
    // Mouse capture makes wheel events arrive as mouse events instead of
    // the arrow keys terminals synthesize in alternate-screen mode.
    crossterm::execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
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
            // stays fresh in the status line between runs.
            _ = ticker.tick(), if app.running() || app.cfg.stats.processes() > 0 => {
                app.spinner_frame = app.spinner_frame.wrapping_add(1);
            }
            // SIGTERM/SIGHUP: leave through the normal quit path so tool
            // destructors kill the child process groups. The guard keeps
            // the completed future from being polled again.
            _ = &mut shutdown, if !app.quit => {
                app.quit = true;
            }
        }
    }

    crossterm::execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    println!(
        "orcacode · session ended · tokens in {} out {}",
        app.tokens_in, app.tokens_out
    );
    Ok(())
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
                MouseEventKind::ScrollUp => app.scroll += 3,
                MouseEventKind::ScrollDown => app.scroll = app.scroll.saturating_sub(3),
                _ => {}
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
        KeyCode::PageUp => app.scroll += SCROLL_PAGE,
        KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(SCROLL_PAGE),
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
        }
        KeyCode::Backspace => {
            if app.cursor > 0 {
                let at = byte_index(&app.composer, app.cursor - 1);
                app.composer.remove(at);
                app.cursor -= 1;
                app.palette_index = 0;
            }
        }
        KeyCode::Delete => {
            if app.cursor < app.composer.chars().count() {
                let at = byte_index(&app.composer, app.cursor);
                app.composer.remove(at);
                app.palette_index = 0;
            }
        }
        KeyCode::Left => app.cursor = app.cursor.saturating_sub(1),
        KeyCode::Right => app.cursor = (app.cursor + 1).min(app.composer.chars().count()),
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.composer.chars().count(),
        KeyCode::Up => {
            if let Some(query) = app.palette_query() {
                let len = filter_commands(query).len();
                if len > 0 {
                    app.palette_index = app.palette_index.min(len - 1).saturating_sub(1);
                }
            } else {
                history_nav(app, -1);
            }
        }
        KeyCode::Down => {
            if let Some(query) = app.palette_query() {
                let len = filter_commands(query).len();
                if len > 0 {
                    app.palette_index = (app.palette_index + 1).min(len - 1);
                }
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
        Overlay::Sessions { sessions, picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => After::CloseAndSend(WorkerCmd::LoadSession {
                path: sessions[index].path.clone(),
            }),
            PickerEvent::Action { key: 'd', row } => {
                let session = &sessions[row];
                if current_session.as_deref() == Some(session.meta.id.as_str()) {
                    After::Note("the active session cannot be deleted (use /clear first)".into())
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
            app.push_line(Line::from(Span::styled(note, theme().dim)));
        }
        After::CloseWithNote(note) => {
            app.overlay = None;
            app.push_line(Line::from(Span::styled(note, theme().dim)));
        }
        After::Note(note) => {
            app.push_line(Line::from(Span::styled(note, theme().dim)));
        }
        After::SendWithNote(cmd, note) => {
            app.overlay = None;
            send_or_report(app, worker, cmd);
            app.push_line(Line::from(Span::styled(note, theme().dim)));
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
                app.push_line(Line::from(Span::styled(
                    "worker is gone; restart orcacode",
                    theme().error,
                )));
            } else {
                app.push_line(Line::from(Span::styled("fetching models…", theme().dim)));
            }
        }
    }
}

fn send_or_report(app: &mut App, worker: &mpsc::UnboundedSender<WorkerCmd>, cmd: WorkerCmd) {
    if worker.send(cmd).is_err() {
        app.push_line(Line::from(Span::styled(
            "worker is gone; restart orcacode",
            theme().error,
        )));
    }
}

/// The highlighted palette entry, if the palette is open and non-empty.
fn palette_selection(app: &App) -> Option<&'static CommandSpec> {
    let query = app.palette_query()?;
    let filtered = filter_commands(query);
    filtered
        .get(app.palette_index.min(filtered.len().saturating_sub(1)))
        .copied()
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
                app.push_line(Line::from(Span::styled(note, theme().dim)));
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

    start_prompt(app, worker, prompt, width);
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
    app.context_tokens += (prompt.len() / 4) as u64;
    if worker
        .send(WorkerCmd::Run {
            prompt: prompt.clone(),
            cancel: cancel.clone(),
        })
        .is_err()
    {
        app.push_line(Line::from(Span::styled(
            "worker is gone; restart orcacode",
            theme().error,
        )));
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
    if !start_prompt(app, worker, prompt, width) {
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
        app.push_line(Line::from(Span::styled("nothing to expand", t.dim)));
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
        app.push_line(Line::from(Span::styled(
            "usage: /queue [clear]",
            theme().error,
        )));
        return;
    }
    if let Some(rest) = command.strip_prefix("expand") {
        let nth = rest.trim().parse::<usize>().unwrap_or(1).max(1);
        expand_tool(app, nth, width);
        return;
    }
    if let Some(rest) = command.strip_prefix("subagents") {
        if rest.is_empty() {
            app.push_line(Line::from(Span::styled(
                format!("subagent nesting depth: {}", app.cfg.subagent_depth.get()),
                dim,
            )));
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
                    app.push_line(Line::from(Span::styled(
                        "usage: /subagents [1-5]",
                        theme().error,
                    )));
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
                    app.push_line(Line::from(Span::styled(
                        "usage: /extensions [enable|disable <name>]",
                        theme().error,
                    )));
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
                app.push_line(Line::from(Span::styled(
                    format!("unknown extension: {name} — valid extensions: {known}"),
                    theme().error,
                )));
                return;
            }
            let state = if enabled { "enabled" } else { "disabled" };
            match crate::config::save_extension(name, enabled) {
                Ok(_) => {
                    app.push_line(Line::from(Span::styled(
                        format!("extension {name} {state} (applies to the next run)"),
                        dim,
                    )));
                    if worker.send(WorkerCmd::ReloadExtensions).is_err() {
                        app.push_line(Line::from(Span::styled(
                            "worker is gone; restart orcacode",
                            theme().error,
                        )));
                    }
                }
                Err(err) => {
                    app.push_line(Line::from(Span::styled(
                        format!("extension {name} not {state} (save failed: {err})"),
                        theme().error,
                    )));
                }
            }
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
                app.push_line(Line::from(Span::styled(
                    "worker is gone; restart orcacode",
                    theme().error,
                )));
            } else {
                app.push_line(Line::from(Span::styled("fetching models…", dim)));
            }
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("sessions") {
        let arg = rest.trim();
        let Some(base) = crate::config::sessions_dir() else {
            app.push_line(Line::from(Span::styled(
                "no home directory for session storage",
                theme().error,
            )));
            return;
        };
        let dir = base.join(orca_harness_extensions::workspace_key(
            &app.cfg.workspace_root,
        ));
        let sessions = orca_harness_extensions::SessionFile::list(&dir);
        if arg.is_empty() {
            if sessions.is_empty() {
                app.push_line(Line::from(Span::styled(
                    "no recorded sessions for this workspace",
                    dim,
                )));
                return;
            }
            // Same interface as /provider and /theme: a picker overlay,
            // preselected on the current session.
            let index = sessions
                .iter()
                .position(|s| app.cfg.session_id.as_deref() == Some(s.meta.id.as_str()))
                .unwrap_or(0);
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
                    app.push_line(Line::from(Span::styled(
                        "worker is gone; restart orcacode",
                        theme().error,
                    )));
                } else {
                    app.push_line(Line::from(Span::styled(
                        format!("loading session {}…", session.meta.id),
                        dim,
                    )));
                }
            }
            None => {
                app.push_line(Line::from(Span::styled(
                    format!("no session matching {arg} — /sessions lists them"),
                    theme().error,
                )));
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
                app.push_line(Line::from(Span::styled(note, dim)));
            }
            None => {
                app.push_line(Line::from(Span::styled(
                    format!(
                        "unknown theme: {arg} — valid themes: default, mono, dracula, solarized-dark, one-dark, monokai, nord"
                    ),
                    theme().error,
                )));
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
                app.push_line(Line::from(Span::styled(
                    "worker is gone; restart orcacode",
                    theme().error,
                )));
            } else {
                app.push_line(Line::from(Span::styled("compacting conversation…", dim)));
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
                "/clear       reset the conversation, start a new session, stop background work",
                "/compact     compact the conversation (elide tool outputs, capped summary)",
                "/usage       session token totals, cache traffic, and context occupancy",
                "/sessions [id] resume a recorded session (no argument opens the picker)",
                "/queue [clear] show or clear waiting prompts",
                "/models [f]  pick a model from the endpoint's catalog",
                "/provider    switch provider (openrouter, openai, local)",
                "/settings    view and change provider, model, theme, api key",
                "/subagents [n] show or set subagent nesting depth (1-5)",
                "/extensions  toggle harness extensions (no argument opens the picker)",
                "/quit        exit",
                "/theme [name] pick a color theme (no argument opens the picker)",
                "keys: enter send or queue · esc cancel run · ctrl+o reveal latest work tree",
                "      pgup/pgdn scroll · ctrl+c quit · up/down history",
                "approvals: y allow once · a always (session) · A always (saved for this workspace) · n deny",
            ] {
                app.push_line(Line::from(Span::styled(entry.to_string(), dim)));
            }
        }
        other => {
            app.push_line(Line::from(Span::styled(
                format!("unknown command: /{other}"),
                theme().error,
            )));
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
            app.push_line(Line::from(Span::styled(text, theme().dim)));
        }
        UiMsg::SessionStarted { id } => {
            app.cfg.session_id = Some(id.clone());
            app.push_line(Line::from(Span::styled(
                format!("new session {id} · background work stopped"),
                theme().dim,
            )));
        }
        UiMsg::SessionLoaded { id, messages } => {
            reset_conversation_ui(app);
            app.cfg.session_id = Some(id.clone());
            app.push_line(Line::from(Span::styled(
                format!("resumed session {id} ({} messages)", messages.len()),
                theme().dim,
            )));
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

/// Build the welcome screen lines. `full_height` is the whole terminal
/// height (including the pinned composer/status rows) so the card sits
/// on the screen's true vertical centre; `clip` is the transcript area
/// height the lines are rendered into, and `top` is capped at it so the
/// card can never be pushed below the visible clip.
fn welcome_lines(
    full_height: usize,
    clip: usize,
    width: usize,
    cfg: &TuiConfig,
) -> Vec<Line<'static>> {
    let t = theme();
    let card_width = width.saturating_sub(4).min(64);
    let indent = " ".repeat(width.saturating_sub(card_width) / 2);
    let value_width = card_width.saturating_sub(11);
    let row = |label: &'static str, value: &str| {
        Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled(format!("{label:<11}"), t.dim),
            Span::raw(view::truncate_line(value, value_width)),
        ])
    };
    let content = vec![
        Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled("orcacode", t.strong),
            Span::styled(format!("  v{}", env!("CARGO_PKG_VERSION")), t.dim),
        ]),
        Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled("A small, fast agent runtime for your terminal", t.dim),
        ]),
        Line::from(""),
        row("model", &cfg.model_name),
        row("workspace", &cfg.workspace_name),
        Line::from(""),
        Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled("› ", t.accent),
            Span::styled("Describe a task to begin", t.strong),
        ]),
        Line::from(vec![
            Span::raw(indent),
            Span::styled("  /help commands · /models switch model", t.dim),
        ]),
    ];
    // Centre against the full terminal height, but never push the bottom
    // of the card past the transcript clip the welcome is drawn into.
    let top =
        (full_height.saturating_sub(content.len()) / 2).min(clip.saturating_sub(content.len()));
    std::iter::repeat_n(Line::from(""), top)
        .chain(content)
        .collect()
}

fn draw(frame: &mut Frame, app: &mut App) {
    let width = frame.area().width as usize;
    // Split the whole terminal first so the transcript, live rail, composer,
    // and status share one left column and the inspector owns the full right.
    let split_active = app.view_mode == ViewMode::Split && width >= 100;
    let [left_root, inspector_area] = if split_active {
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
            .areas(frame.area())
    } else {
        [frame.area(), frame.area()]
    };
    let left_width = left_root.width as usize;
    let live = live_lines(app, left_width);
    let live_height = live.len().min(PALETTE_ROWS + 7) as u16;
    let [transcript_area, live_area, _composer_gap_area, composer_area, status_area] =
        Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(live_height),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(left_root);

    // Transcript: committed history plus a render-only projection of the
    // in-progress turn. Deltas therefore appear in their final location
    // instead of streaming through the temporary area and jumping here.
    let height = transcript_area.height as usize;
    let transcript_width = transcript_area.width as usize;
    let selected_tool = (split_active && !app.activity_tools.is_empty()).then(|| {
        app.split_tool
            .unwrap_or_else(|| app.activity_tools.len().saturating_sub(1))
            .min(app.activity_tools.len().saturating_sub(1))
    });
    let projected = if selected_tool.is_some() {
        projected_transcript_selected(app, transcript_width, selected_tool)
    } else {
        projected_transcript(app, transcript_width)
    };
    if projected.is_empty() && !app.running() {
        let full_height = frame.area().height as usize;
        let welcome = welcome_lines(full_height, height, transcript_width, &app.cfg);
        frame.render_widget(Paragraph::new(Text::from(welcome)), transcript_area);
    } else {
        let max_scroll = projected.len().saturating_sub(height);
        stabilize_transcript_scroll(app, max_scroll);
        let end = projected.len().saturating_sub(app.scroll);
        let start = end.saturating_sub(height);
        frame.render_widget(
            Paragraph::new(Text::from(projected[start..end].to_vec())),
            transcript_area,
        );
    }

    if split_active {
        let inspected = selected_tool
            .and_then(|selected| app.activity_tools.get(selected))
            .or(app.split_snapshot.as_ref());
        let inspector_width = inspector_area.width as usize;
        let (header, body) = if let Some(tool) = inspected {
            let complete = tool.output.is_some();
            let cache_valid = app.split_inspector_cache.as_ref().is_some_and(|cache| {
                cache.call_id == tool.call_id
                    && cache.complete == complete
                    && cache.is_error == tool.is_error
                    && cache.width == inspector_width
            });
            if !cache_valid {
                app.split_inspector_cache = Some(InspectorBodyCache {
                    call_id: tool.call_id.clone(),
                    complete,
                    is_error: tool.is_error,
                    width: inspector_width,
                    lines: tool_inspector_body_lines(tool, inspector_width),
                });
            }
            (
                tool_inspector_header_lines(tool),
                app.split_inspector_cache
                    .as_ref()
                    .map(|cache| cache.lines.clone())
                    .unwrap_or_default(),
            )
        } else {
            (empty_tool_inspector_lines(), Vec::new())
        };
        let header_len = header.len();
        let [header_area, body_area] =
            Layout::vertical([Constraint::Length(header_len as u16), Constraint::Min(0)])
                .areas(inspector_area);
        let max_scroll = body
            .len()
            .saturating_sub(body_area.height as usize)
            .min(u16::MAX as usize) as u16;
        app.split_scroll = app.split_scroll.min(max_scroll);
        let border_style = if app.split_focused {
            theme().accent
        } else {
            theme().dim
        };
        let divider = || {
            Block::default()
                .borders(Borders::LEFT)
                .border_style(border_style)
        };
        frame.render_widget(
            Paragraph::new(Text::from(header)).block(divider()),
            header_area,
        );
        let body_start = app.split_scroll as usize;
        let body_end = (body_start + body_area.height as usize).min(body.len());
        frame.render_widget(
            Paragraph::new(Text::from(body[body_start..body_end].to_vec())).block(divider()),
            body_area,
        );
    }

    frame.render_widget(Paragraph::new(Text::from(live)), live_area);

    // Composer with a horizontally-scrolling single line and a
    // placeholder when empty.
    let inner_width = (composer_area.width as usize).saturating_sub(3).max(8);
    let chars: Vec<char> = app.composer.chars().collect();
    let start = if app.cursor >= inner_width {
        app.cursor + 1 - inner_width
    } else {
        0
    };
    let visible: String = chars.iter().skip(start).take(inner_width).collect();
    let composer_line = if app.composer.is_empty() {
        let placeholder = if app.running() {
            "type another prompt to queue"
        } else if !app.prompt_queue.is_empty() {
            "queue paused · enter to resume"
        } else {
            "ask anything · /help for commands"
        };
        Line::from(vec![
            Span::styled("│ ", theme().accent),
            Span::styled(placeholder, theme().dim),
        ])
    } else {
        Line::from(vec![Span::styled("│ ", theme().accent), Span::raw(visible)])
    };
    frame.render_widget(Paragraph::new(composer_line), composer_area);
    frame.set_cursor_position((
        composer_area.x + 2 + (app.cursor - start) as u16,
        composer_area.y,
    ));

    // Status line with contextual hints.
    let state = if app.approval.is_some() {
        "awaiting approval"
    } else if app.running() {
        "running"
    } else if !app.prompt_queue.is_empty() {
        "queue paused"
    } else {
        "idle"
    };
    let hint = if app.scroll > 0 {
        "scrolled · pgdn to follow"
    } else if app.overlay.is_some() && app.approval.is_none() {
        "↑↓ navigate · enter use · esc close"
    } else if app.palette_query().is_some() && app.approval.is_none() {
        "↑↓ navigate · enter use · tab complete · esc close"
    } else if app.split_focused {
        "↑↓ select tool · pgup/pgdn scroll · tab return"
    } else if split_active && !app.activity_tools.is_empty() {
        "tab inspect · enter queue · esc interrupt"
    } else if split_active && app.running() {
        "split ready · waiting for tool call · esc interrupt"
    } else if app.running() {
        "enter queue · esc interrupt"
    } else if !app.prompt_queue.is_empty() {
        "enter resume · /queue clear"
    } else {
        "enter send · ctrl+o expand · pgup scroll"
    };
    let status = format!(
        " {} · {} · {}{}{} · {} · {}",
        app.cfg.model_name,
        state,
        context_segment(app.context_tokens, app.context_window),
        stats_segments(&app.cfg.stats),
        queue_segment(app.prompt_queue.len()),
        hint,
        workspace_status_name(&app.cfg.workspace_name),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            view::truncate_line(&status, left_width),
            theme().dim,
        ))),
        status_area,
    );
}

fn transcript_content_width(app: &App, terminal_width: usize) -> usize {
    if app.view_mode == ViewMode::Split && terminal_width >= 100 {
        terminal_width.saturating_mul(58) / 100
    } else {
        terminal_width
    }
}

fn stabilize_transcript_scroll(app: &mut App, max_scroll: usize) {
    if app.scroll > 0 {
        if max_scroll >= app.transcript_max_scroll {
            app.scroll = app
                .scroll
                .saturating_add(max_scroll - app.transcript_max_scroll);
        } else {
            app.scroll = app
                .scroll
                .saturating_sub(app.transcript_max_scroll - max_scroll);
        }
    }
    app.scroll = app.scroll.min(max_scroll);
    app.transcript_max_scroll = max_scroll;
}

/// Keep the pinned status line compact by showing only the workspace folder.
/// The welcome screen still shows the full path.
fn workspace_status_name(workspace: &str) -> &str {
    Path::new(workspace)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(workspace)
}

#[cfg(test)]
fn tool_inspector_lines(tool: &ToolActivity, width: usize) -> Vec<Line<'static>> {
    let mut lines = tool_inspector_header_lines(tool);
    lines.extend(tool_inspector_body_lines(tool, width));
    lines
}

fn tool_inspector_header_lines(tool: &ToolActivity) -> Vec<Line<'static>> {
    let t = theme();
    let elapsed = tool.elapsed.unwrap_or_else(|| tool.started.elapsed());
    let (status, status_style) = match &tool.output {
        Some(_) if tool.is_error => ("failed", t.error),
        Some(_) => ("completed", t.success),
        None => ("running", t.accent),
    };
    vec![
        Line::from(vec![
            Span::styled("  ○ ", t.dim),
            Span::styled(tool.tool_name.to_uppercase(), t.strong),
        ]),
        Line::from(vec![
            Span::styled(format!("  {status}"), status_style),
            Span::styled(format!(" · {}", elapsed_label(elapsed)), t.dim),
        ]),
        Line::from(""),
    ]
}

fn tool_inspector_body_lines(tool: &ToolActivity, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let inner = width.saturating_sub(4).max(16);
    let mut lines = vec![Line::from(Span::styled("  › input", t.dim))];
    append_inspector_input(&mut lines, tool, inner);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("  · output", t.dim)));
    if let Some(output) = &tool.output {
        let language = inspector_output_language(tool, output);
        if let Some(facts) = inspector_code_facts(tool, output, language) {
            lines.push(Line::from(Span::styled(format!("  {facts}"), t.dim)));
        }
        let (expanded, omitted) =
            inspector_output_preview(&tool.tool_name, output, language, tool.is_error);
        if tool.is_error {
            push_inspector_text(&mut lines, &expanded, inner, t.error);
        } else if let Some(language) = language {
            lines.extend(view::highlighted_code_lines(
                &expanded, language, inner, "  ",
            ));
        } else {
            push_inspector_text(&mut lines, &expanded, inner, Style::default());
        }
        if omitted {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  More output omitted · /expand n shows the full result",
                t.dim,
            )));
        }
    } else {
        lines.push(Line::from(Span::styled("  Waiting for result", t.dim)));
    }
    lines
}

fn append_inspector_input(lines: &mut Vec<Line<'static>>, tool: &ToolActivity, width: usize) {
    let path = tool.input.get("path").and_then(serde_json::Value::as_str);
    let content = tool
        .input
        .get("content")
        .and_then(serde_json::Value::as_str);
    if let (Some(path), Some(content)) = (path, content) {
        let language = language_for_path(path).unwrap_or("text");
        let line_count = content.lines().count();
        let line_label = if line_count == 1 { "line" } else { "lines" };
        lines.push(Line::from(Span::styled(
            format!(
                "  {path} · {language} · {line_count} {line_label} · {}",
                inspector_size_label(content.len() as u64)
            ),
            theme().dim,
        )));
        let (preview, omitted) = limit_inspector_preview(content);
        if language == "text" {
            push_inspector_text(lines, &preview, width, Style::default());
        } else {
            lines.extend(view::highlighted_code_lines(
                &preview, language, width, "  ",
            ));
        }
        if omitted {
            lines.push(Line::from(Span::styled(
                "  More input omitted",
                theme().dim,
            )));
        }
        return;
    }

    let input =
        serde_json::to_string_pretty(&tool.input).unwrap_or_else(|_| tool.input.to_string());
    lines.extend(view::highlighted_code_lines(&input, "json", width, "  "));
}

fn inspector_code_facts(
    tool: &ToolActivity,
    output: &serde_json::Value,
    language: Option<&str>,
) -> Option<String> {
    if tool.tool_name != "read_file" || tool.is_error {
        return None;
    }
    let content = output.get("content")?.as_str()?;
    let language = language?;
    let lines = content.lines().count();
    let bytes = output
        .get("bytes")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(content.len() as u64);
    let truncated = output
        .get("truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let range = match lines {
        0 => "empty".to_owned(),
        1 => "1 line".to_owned(),
        count => format!("{count} lines"),
    };
    let suffix = if truncated { " · truncated" } else { "" };
    Some(format!(
        "{language} · {range} · {}{suffix}",
        inspector_size_label(bytes)
    ))
}

fn inspector_size_label(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn shallow_json_preview(output: &serde_json::Value) -> (String, bool) {
    fn child(value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::Object(_) => "{ … }".to_owned(),
            serde_json::Value::Array(_) => "[ … ]".to_owned(),
            _ => serde_json::to_string(value).unwrap_or_else(|_| value.to_string()),
        }
    }

    let max_children = INSPECTOR_PREVIEW_LINES.saturating_sub(2);
    let (text, omitted) = match output {
        serde_json::Value::Object(map) => {
            let shown = map.len().min(max_children);
            let mut lines = Vec::with_capacity(shown + 2);
            lines.push("{".to_owned());
            for (index, (key, value)) in map.iter().take(shown).enumerate() {
                let comma = if index + 1 < shown { "," } else { "" };
                let key = serde_json::to_string(key).unwrap_or_else(|_| format!("\"{key}\""));
                lines.push(format!("  {key}: {}{comma}", child(value)));
            }
            lines.push("}".to_owned());
            (lines.join("\n"), map.len() > shown)
        }
        serde_json::Value::Array(values) => {
            let shown = values.len().min(max_children);
            let mut lines = Vec::with_capacity(shown + 2);
            lines.push("[".to_owned());
            for (index, value) in values.iter().take(shown).enumerate() {
                let comma = if index + 1 < shown { "," } else { "" };
                lines.push(format!("  {}{comma}", child(value)));
            }
            lines.push("]".to_owned());
            (lines.join("\n"), values.len() > shown)
        }
        _ => (
            serde_json::to_string_pretty(output).unwrap_or_else(|_| output.to_string()),
            false,
        ),
    };
    let (text, size_omitted) = limit_inspector_preview(&text);
    (text, omitted || size_omitted)
}

fn inspector_output_preview(
    tool_name: &str,
    output: &serde_json::Value,
    language: Option<&str>,
    is_error: bool,
) -> (String, bool) {
    if is_error {
        return (view::tool_result_summary(tool_name, output, true), false);
    }
    match classify_inspector_output(output, language) {
        InspectorOutputKind::Execution => shell_inspector_preview(output),
        InspectorOutputKind::Collection { field, label } => {
            collection_inspector_preview(output, field, label)
        }
        InspectorOutputKind::Process => process_inspector_preview(output),
        InspectorOutputKind::Mutation => mutation_inspector_preview(output),
        InspectorOutputKind::Structured => shallow_json_preview(output),
        InspectorOutputKind::Text => {
            let expanded = inspector_text_content(output);
            limit_inspector_preview(&expanded)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InspectorOutputKind {
    Execution,
    Collection {
        field: &'static str,
        label: &'static str,
    },
    Process,
    Mutation,
    Structured,
    Text,
}

fn classify_inspector_output(
    output: &serde_json::Value,
    language: Option<&str>,
) -> InspectorOutputKind {
    if output.get("stdout").is_some() || output.get("stderr").is_some() {
        InspectorOutputKind::Execution
    } else if output
        .get("entries")
        .is_some_and(serde_json::Value::is_array)
    {
        InspectorOutputKind::Collection {
            field: "entries",
            label: "entries",
        }
    } else if output
        .get("matches")
        .is_some_and(serde_json::Value::is_array)
    {
        InspectorOutputKind::Collection {
            field: "matches",
            label: "matches",
        }
    } else if output
        .get("processes")
        .is_some_and(serde_json::Value::is_array)
        || (output.get("id").is_some()
            && (output.get("running").is_some() || output.get("output").is_some()))
    {
        InspectorOutputKind::Process
    } else if output.get("bytesWritten").is_some() || output.get("replacements").is_some() {
        InspectorOutputKind::Mutation
    } else if output.is_string()
        || output
            .get("content")
            .is_some_and(serde_json::Value::is_string)
    {
        InspectorOutputKind::Text
    } else if language == Some("json") || output.is_object() || output.is_array() {
        InspectorOutputKind::Structured
    } else {
        InspectorOutputKind::Text
    }
}

fn inspector_text_content(output: &serde_json::Value) -> String {
    output
        .as_str()
        .or_else(|| output.get("content").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .unwrap_or_else(|| output.to_string())
}

fn mutation_inspector_preview(output: &serde_json::Value) -> (String, bool) {
    let path = output
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(|path| format!("{path} · "))
        .unwrap_or_default();
    if let Some(bytes) = output
        .get("bytesWritten")
        .and_then(serde_json::Value::as_u64)
    {
        return (
            format!("{path}wrote {}", inspector_size_label(bytes)),
            false,
        );
    }
    if let Some(replacements) = output
        .get("replacements")
        .and_then(serde_json::Value::as_u64)
    {
        let label = if replacements == 1 {
            "replacement"
        } else {
            "replacements"
        };
        return (format!("{path}{replacements} {label}"), false);
    }
    shallow_json_preview(output)
}

fn shell_inspector_preview(output: &serde_json::Value) -> (String, bool) {
    let exit = output
        .get("exitCode")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_default();
    let stdout = output
        .get("stdout")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let stderr = output
        .get("stderr")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let stdout_count = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    let stderr_count = stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    let mut text = format!("exit {exit}");
    if stdout_count > 0 {
        text.push_str(&format!(" · {stdout_count} stdout"));
    }
    if stderr_count > 0 {
        text.push_str(&format!(" · {stderr_count} stderr"));
    }
    let mut detail: Vec<&str> = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|line| !line.trim().is_empty())
        .collect();
    detail.dedup_by(|a, b| a.trim() == b.trim());
    let (shown, omitted) = informative_line_window(&detail);
    if !shown.is_empty() {
        text.push('\n');
        text.push_str(&shown.join("\n"));
    }
    let (text, size_omitted) = limit_inspector_preview(&text);
    (text, omitted || size_omitted)
}

fn collection_inspector_preview(
    output: &serde_json::Value,
    field: &str,
    label: &str,
) -> (String, bool) {
    let Some(items) = output.get(field).and_then(serde_json::Value::as_array) else {
        return shallow_json_preview(output);
    };
    let values: Vec<String> = items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| item.to_string())
        })
        .collect();
    let refs: Vec<&str> = values.iter().map(String::as_str).collect();
    let (shown, window_omitted) = informative_line_window(&refs);
    let truncated = output
        .get("truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let mut text = format!("{} {label}", items.len());
    if !shown.is_empty() {
        text.push('\n');
        text.push_str(&shown.join("\n"));
    }
    (text, truncated || window_omitted)
}

fn process_inspector_preview(output: &serde_json::Value) -> (String, bool) {
    let mut text = view::tool_result_summary("process", output, false);
    let detail = output
        .get("output")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let lines: Vec<&str> = detail
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .collect();
    let (shown, omitted) = informative_line_window(&lines);
    if !shown.is_empty() {
        text.push('\n');
        text.push_str(&shown.join("\n"));
    }
    let (text, size_omitted) = limit_inspector_preview(&text);
    (text, omitted || size_omitted)
}

fn informative_line_window<'a>(lines: &[&'a str]) -> (Vec<&'a str>, bool) {
    let limit = INSPECTOR_OUTPUT_HEAD + INSPECTOR_OUTPUT_TAIL;
    if lines.len() <= limit {
        return (lines.to_vec(), false);
    }
    let mut shown = lines[..INSPECTOR_OUTPUT_HEAD].to_vec();
    shown.extend_from_slice(&lines[lines.len() - INSPECTOR_OUTPUT_TAIL..]);
    (shown, true)
}

fn limit_inspector_preview(expanded: &str) -> (String, bool) {
    let mut preview = String::new();
    let mut omitted = expanded.lines().count() > INSPECTOR_PREVIEW_LINES;
    for (index, line) in expanded.lines().take(INSPECTOR_PREVIEW_LINES).enumerate() {
        let separator = usize::from(index > 0);
        let remaining = INSPECTOR_PREVIEW_CHARS.saturating_sub(preview.len() + separator);
        if remaining == 0 {
            omitted = true;
            break;
        }
        if index > 0 {
            preview.push('\n');
        }
        if line.len() > remaining {
            let end = line
                .char_indices()
                .map(|(offset, _)| offset)
                .take_while(|offset| *offset <= remaining)
                .last()
                .unwrap_or(0);
            preview.push_str(&line[..end]);
            omitted = true;
            break;
        }
        preview.push_str(line);
    }
    (preview, omitted)
}

fn inspector_output_language(
    tool: &ToolActivity,
    output: &serde_json::Value,
) -> Option<&'static str> {
    let path = tool
        .input
        .get("path")
        .or_else(|| tool.input.get("file_path"))
        .and_then(serde_json::Value::as_str);
    if let Some(language) = path.and_then(language_for_path) {
        return Some(language);
    }
    if (output.is_object() || output.is_array())
        && !matches!(
            tool.tool_name.as_str(),
            "shell" | "process" | "grep" | "read_file" | "list_dir"
        )
    {
        Some("json")
    } else {
        None
    }
}

fn language_for_path(path: &str) -> Option<&'static str> {
    let extension = Path::new(path).extension()?.to_str()?;
    match extension.to_ascii_lowercase().as_str() {
        "rs" => Some("rust"),
        "js" | "jsx" => Some("javascript"),
        "ts" | "tsx" => Some("typescript"),
        "py" => Some("python"),
        "go" => Some("go"),
        "sh" | "bash" | "zsh" => Some("bash"),
        "json" => Some("json"),
        "toml" => Some("toml"),
        "yaml" | "yml" => Some("yaml"),
        "md" => Some("markdown"),
        "html" => Some("html"),
        "css" => Some("css"),
        "sql" => Some("sql"),
        _ => None,
    }
}

fn empty_tool_inspector_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled("  TOOL INSPECTOR", theme().strong)),
        Line::from(""),
        Line::from(Span::styled(
            "  Tool input and output will appear here.",
            theme().dim,
        )),
    ]
}

fn push_inspector_text(lines: &mut Vec<Line<'static>>, text: &str, width: usize, style: Style) {
    for source in text.lines() {
        let wrapped = textwrap::wrap(source, width.saturating_sub(2).max(8));
        if wrapped.is_empty() {
            lines.push(Line::from(""));
        } else {
            for part in wrapped {
                lines.push(Line::from(Span::styled(format!("  {part}"), style)));
            }
        }
    }
}

/// Status-line segments for live background work; empty when idle so the
/// line stays quiet. `pykernel` is unnumbered (it is 0 or 1).
fn stats_segments(stats: &orca_harness_tools::BackgroundStats) -> String {
    let mut out = String::new();
    if stats.processes() > 0 {
        out.push_str(&format!(" · procs {}", stats.processes()));
    }
    if stats.kernels() > 0 {
        out.push_str(" · pykernel");
    }
    if stats.agents() > 0 {
        out.push_str(&format!(" · agents {}", stats.agents()));
    }
    out
}

fn queue_segment(queued: usize) -> String {
    if queued == 0 {
        String::new()
    } else {
        format!(" · queued {queued}")
    }
}

/// A compact execution rail. Prompts stay out of the transcript until
/// they start, so the conversation preserves its actual chronology.
fn queue_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    if app.prompt_queue.is_empty() {
        return Vec::new();
    }

    let mut lines = vec![Line::from(vec![
        Span::styled("  queued", t.strong),
        Span::styled(format!(" · {}", app.prompt_queue.len()), t.dim),
    ])];
    let visible = app.prompt_queue.len().min(QUEUE_PREVIEW_ROWS);
    let overflow = app.prompt_queue.len().saturating_sub(visible);
    for (index, prompt) in app.prompt_queue.iter().take(visible).enumerate() {
        let last = index + 1 == visible && overflow == 0;
        let branch = if last { "└" } else { "├" };
        let label = if index == 0 {
            "next".to_string()
        } else {
            (index + 1).to_string()
        };
        let available = width.saturating_sub(12).max(8);
        lines.push(Line::from(vec![
            Span::styled(format!("  {branch} "), t.dim),
            Span::styled(
                format!("{label:<4} "),
                if index == 0 { t.accent } else { t.dim },
            ),
            Span::styled(
                view::truncate_line(prompt, available),
                if index == 0 { t.strong } else { t.dim },
            ),
        ]));
    }
    if overflow > 0 {
        lines.push(Line::from(Span::styled(
            format!("  └      +{overflow} more"),
            t.dim,
        )));
    }
    lines
}

/// The pinned live region: approval prompt beats palette beats run status.
/// Streaming content itself is projected into the main transcript.
fn live_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    if let Some(request) = &app.approval {
        return vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("  approval required: {}", request.tool_name),
                t.warn,
            )),
            Line::from(Span::raw(format!(
                "    {}",
                view::truncate_line(&request.detail, width.saturating_sub(6))
            ))),
            Line::from(Span::styled(
                "    [y] allow once   [a] always (this session)   [A] always (save for workspace)   [n] deny",
                t.dim,
            )),
        ];
    }
    if let Some(overlay) = &app.overlay {
        return match overlay {
            Overlay::Models(picker) => model_picker_lines(picker, PICKER_ROWS + 2, width),
            Overlay::Providers { picker } => provider_lines(picker, width),
            Overlay::Themes { picker } => theme_picker_lines(picker, width),
            Overlay::Views { picker } => view_picker_lines(app.view_mode, picker, width),
            Overlay::Usage => usage_lines(app, width),
            Overlay::ApiKey { provider, input } => api_key_lines(*provider, input),
            Overlay::Settings { picker } => settings_lines(app, picker, width),
            Overlay::Approvals { tools, picker } => approvals_lines(tools, picker, width),
            Overlay::Extensions { picker } => extensions_picker_lines(picker, width),
            Overlay::Sessions { sessions, picker } => {
                sessions_picker_lines(sessions, app.cfg.session_id.as_deref(), picker, width)
            }
        };
    }
    if app.palette_query().is_some() {
        return palette_lines(app, PALETTE_ROWS + 2, width);
    }
    if app.running() {
        let mut lines = queue_lines(app, width);
        let spinner = SPINNER[app.spinner_frame % SPINNER.len()];
        let verb = if !app.text.is_empty() || app.pending_assistant.is_some() {
            "writing"
        } else if !app.reasoning.is_empty() {
            "thinking"
        } else {
            "working"
        };
        if let RunState::Running { started, .. } = &app.run {
            lines.push(Line::from(vec![
                Span::styled(format!("  {spinner} "), t.accent),
                Span::styled(
                    format!(
                        "{verb} · {}s · esc to interrupt",
                        started.elapsed().as_secs()
                    ),
                    t.dim,
                ),
            ]));
        }
        return lines;
    }
    let mut lines = queue_lines(app, width);
    if let Some(summary) = &app.last_turn_summary {
        lines.push(Line::from(Span::styled(format!("  {summary}"), t.dim)));
    }
    lines
}

fn projected_transcript(app: &App, width: usize) -> Vec<Line<'static>> {
    projected_transcript_selected(app, width, None)
}

fn projected_transcript_selected(
    app: &App,
    width: usize,
    selected_tool: Option<usize>,
) -> Vec<Line<'static>> {
    let mut lines = app.transcript.clone();
    if !app.running() {
        return lines;
    }

    let activity = activity_lines_selected(app, width, true, selected_tool);
    let answer = if !app.text.is_empty() {
        Some(app.text.as_str())
    } else {
        app.pending_assistant
            .as_deref()
            .filter(|text| !text.is_empty())
    };
    if activity.is_empty() && answer.is_none() {
        return lines;
    }

    let has_activity = !activity.is_empty();
    append_render_block(&mut lines, activity, BlockSpacing::Section, None);
    if let Some(answer) = answer {
        let spacing = if has_activity {
            BlockSpacing::Tight
        } else {
            BlockSpacing::Section
        };
        append_render_block(
            &mut lines,
            view::markdown_lines(answer, width, "  "),
            spacing,
            None,
        );
    }
    lines
}

fn elapsed_label(elapsed: Duration) -> String {
    if elapsed.as_secs() > 0 {
        format!("{:.1}s", elapsed.as_secs_f64())
    } else if elapsed.as_millis() > 0 {
        format!("{}ms", elapsed.as_millis())
    } else if elapsed.as_micros() > 0 {
        format!("{}µs", elapsed.as_micros())
    } else {
        format!("{}ns", elapsed.as_nanos())
    }
}

fn plural(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

/// Cap on rendered inner tool rows per spawn while live.
const NESTED_TOOL_ROWS: usize = 4;

/// Inner tool rows for every spawn anchored to `call_id`, plus their
/// descendants. Rows reuse the rail's `├─`/`└─` vocabulary one level
/// deeper, and `continuation` carries the parent rail's `│ ` (or blank)
/// so ownership stays unambiguous even mid-list.
fn nested_subagent_lines(
    app: &App,
    call_id: &str,
    width: usize,
    continuation: &str,
    lines: &mut Vec<Line<'static>>,
) {
    let mut roots: Vec<u64> = app
        .subagent_activity
        .iter()
        .filter(|(_, spawn)| spawn.call_id == call_id)
        .map(|(id, _)| *id)
        .collect();
    roots.sort_unstable();
    let prefix = format!("    {continuation} ");
    for id in roots {
        nested_spawn_rows(app, id, width, &prefix, lines);
    }
}

fn nested_spawn_rows(
    app: &App,
    id: u64,
    width: usize,
    prefix: &str,
    lines: &mut Vec<Line<'static>>,
) {
    let Some(spawn) = app.subagent_activity.get(&id) else {
        return;
    };
    let t = theme();
    let hidden = spawn.tools.len().saturating_sub(NESTED_TOOL_ROWS);
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("{prefix}… {hidden} earlier tools"),
            t.dim,
        )));
    }
    let visible: Vec<&ToolActivity> = spawn.tools.iter().skip(hidden).collect();
    for (position, tool) in visible.iter().enumerate() {
        let last = position + 1 == visible.len();
        let branch = if last { "└─" } else { "├─" };
        let elapsed = tool.elapsed.unwrap_or_else(|| tool.started.elapsed());
        let (glyph, style) = match &tool.output {
            Some(_) if tool.is_error => ("×", t.error),
            Some(_) => ("✓", t.dim),
            None => ("□", t.dim),
        };
        let call = view::truncate_line(
            &tool.call_line,
            width.saturating_sub(prefix.len() + 20).max(8),
        );
        lines.push(Line::from(vec![
            Span::styled(format!("{prefix}{branch} "), t.dim),
            Span::styled(format!("{glyph} "), style),
            Span::styled(call, t.accent),
            Span::styled(format!(" · {}", elapsed_label(elapsed)), t.dim),
        ]));
        // A running nested subagent call: its spawns branch off this row.
        if tool.tool_name == "subagent" && tool.output.is_none() {
            let child_prefix = format!("{prefix}{}  ", if last { " " } else { "│" });
            let mut children: Vec<u64> = app
                .subagent_activity
                .iter()
                .filter(|(_, s)| s.parent_id == Some(id))
                .map(|(child, _)| *child)
                .collect();
            children.sort_unstable();
            for child in children {
                nested_spawn_rows(app, child, width, &child_prefix, lines);
            }
        }
    }
}

/// Quiet, chronological rows for a completed phase. Thinking and tool work
/// remain separate so collapsing detail never rewrites the event sequence.
fn collapsed_activity_lines(app: &App) -> Vec<Line<'static>> {
    let tool_count = app.activity_tools.len();
    let thinking_count = app.thinking_log.len();
    let failed = app
        .activity_tools
        .iter()
        .filter(|tool| tool.is_error)
        .count();
    let tool_elapsed = app
        .activity_tools
        .iter()
        .filter_map(|tool| tool.elapsed)
        .max()
        .unwrap_or_default();

    let mut lines = Vec::new();
    if thinking_count > 0 {
        let thinking_elapsed = app
            .thinking_log
            .iter()
            .map(|record| record.elapsed)
            .sum::<Duration>();
        lines.push(Line::from(Span::styled(
            format!(
                "  • Thinking · {} · {}",
                elapsed_label(thinking_elapsed),
                plural(thinking_count, "update")
            ),
            theme().dim,
        )));
    }

    if tool_count > 0 {
        let mut parts = vec![plural(tool_count, "tool")];
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        parts.push(elapsed_label(tool_elapsed));
        let style = if failed > 0 {
            theme().error
        } else {
            theme().dim
        };
        lines.push(Line::from(Span::styled(
            format!("  • Work · {}", parts.join(" · ")),
            style,
        )));
    }

    lines
}

/// Render the current run as one coherent activity rail. While the run is
/// live this includes the latest reasoning tail and pending tool states;
/// once committed, the rail is retained for on-demand expansion.
#[cfg(test)]
fn activity_lines(app: &App, width: usize, live: bool) -> Vec<Line<'static>> {
    activity_lines_selected(app, width, live, None)
}

fn activity_lines_selected(
    app: &App,
    width: usize,
    live: bool,
    selected_tool: Option<usize>,
) -> Vec<Line<'static>> {
    let t = theme();
    let mut lines = Vec::new();
    let current_thinking = !app.reasoning.trim().is_empty();
    let thinking_count = app.thinking_log.len() + usize::from(current_thinking);
    if thinking_count > 0 {
        let elapsed = app
            .thinking_log
            .iter()
            .map(|record| record.elapsed)
            .sum::<Duration>()
            + app
                .reasoning_started
                .map(|started| started.elapsed())
                .unwrap_or_default();
        let marker = if live {
            if app.spinner_frame.is_multiple_of(2) {
                "•"
            } else {
                " "
            }
        } else {
            "•"
        };
        lines.push(Line::from(Span::styled(
            format!(
                "  {marker} Thinking · {} · {}",
                elapsed_label(elapsed),
                plural(thinking_count, "update")
            ),
            t.dim,
        )));
        if live && current_thinking {
            let body_width = width.saturating_sub(6).max(16);
            let wrapped: Vec<String> = app
                .reasoning
                .lines()
                .flat_map(|paragraph| {
                    textwrap::wrap(paragraph, body_width)
                        .into_iter()
                        .map(|part| part.into_owned())
                })
                .collect();
            for line in wrapped.iter().rev().take(2).rev() {
                lines.push(Line::from(Span::styled(format!("    {line}"), t.dim)));
            }
        }
    }

    if app.activity_tools.is_empty() {
        return lines;
    }
    let complete = app
        .activity_tools
        .iter()
        .filter(|tool| tool.output.is_some())
        .count();
    let running = app.activity_tools.len() - complete;
    let header = if live {
        let dot = if app.spinner_frame.is_multiple_of(2) {
            "•"
        } else {
            " "
        };
        format!("  {dot} Work · ✓ {complete} · □ {running}")
    } else {
        format!("  • Work · {}", plural(app.activity_tools.len(), "tool"))
    };
    lines.push(Line::from(Span::styled(header, t.dim)));

    let visible_indices = if live && app.activity_tools.len() > LIVE_TOOL_ROWS {
        let mut selected: Vec<usize> = app
            .activity_tools
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, tool)| tool.output.is_none())
            .map(|(index, _)| index)
            .take(LIVE_TOOL_ROWS)
            .collect();
        let remaining = LIVE_TOOL_ROWS.saturating_sub(selected.len());
        selected.extend(
            app.activity_tools
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, tool)| tool.output.is_some())
                .map(|(index, _)| index)
                .take(remaining),
        );
        selected.sort_unstable();
        selected
    } else {
        (0..app.activity_tools.len()).collect()
    };
    let hidden = app
        .activity_tools
        .len()
        .saturating_sub(visible_indices.len());
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("    … {hidden} earlier tools"),
            t.dim,
        )));
    }

    for (position, index) in visible_indices.iter().copied().enumerate() {
        let tool = &app.activity_tools[index];
        let last = position + 1 == visible_indices.len();
        let branch = if last { "└─" } else { "├─" };
        let continuation = if last { "  " } else { "│ " };
        let elapsed = tool.elapsed.unwrap_or_else(|| tool.started.elapsed());
        let (glyph, mut detail, status_style) = match &tool.output {
            Some(output) if tool.is_error => (
                "×",
                format!(
                    "{} · {}",
                    view::tool_result_summary(&tool.tool_name, output, true),
                    elapsed_label(elapsed)
                ),
                t.error,
            ),
            Some(output) => (
                "✓",
                format!(
                    "{} · {}",
                    view::tool_result_summary(&tool.tool_name, output, false),
                    elapsed_label(elapsed)
                ),
                t.dim,
            ),
            None if live => ("□", elapsed_label(elapsed), t.dim),
            None => ("×", elapsed_label(elapsed), t.warn),
        };
        if let Some(approval) = &tool.approval {
            detail = format!("{approval} · {detail}");
        }
        let row_width = width.min(132);
        let detail_width = (row_width / 3).clamp(16, 48);
        detail = view::truncate_line(&detail, detail_width);
        let selected = selected_tool == Some(index);
        let prefix = format!("    {branch} ");
        let status = format!(" · {detail}");
        let fixed_width = prefix.chars().count() + 2 + status.chars().count();
        let connector_reserve = if selected { 10 } else { 0 };
        let call_width = row_width
            .saturating_sub(fixed_width)
            .min(width.saturating_sub(fixed_width + connector_reserve))
            .max(8);
        let call = view::truncate_line(&tool.call_line, call_width);
        let row_style = if selected { t.strong } else { t.accent };
        let mut spans = vec![
            Span::styled(prefix, t.dim),
            Span::styled(format!("{glyph} "), status_style),
            Span::styled(call, row_style),
            Span::styled(status, status_style),
        ];
        if selected {
            let used = spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum::<usize>();
            let dots = width.saturating_sub(used + 1);
            spans.push(Span::styled(
                format!(" {}○", "·".repeat(dots.saturating_sub(1).max(1))),
                t.dim,
            ));
        }
        lines.push(Line::from(spans));
        if tool.tool_name == "edit_file" {
            let diff_width = width.saturating_sub(12).max(16);
            if let Some(old) = tool.input.get("old").and_then(serde_json::Value::as_str) {
                lines.push(Line::from(Span::styled(
                    format!(
                        "    {continuation} - {}",
                        view::truncate_line(old, diff_width)
                    ),
                    t.dim,
                )));
            }
            if let Some(new) = tool.input.get("new").and_then(serde_json::Value::as_str) {
                lines.push(Line::from(Span::styled(
                    format!(
                        "    {continuation} + {}",
                        view::truncate_line(new, diff_width)
                    ),
                    t.success,
                )));
            }
        }
        if tool.tool_name == "subagent" && tool.output.is_none() {
            nested_subagent_lines(app, &tool.call_id, width, continuation, &mut lines);
        }
        if tool.is_error {
            if let Some(output) = &tool.output {
                let output_width = width.saturating_sub(12).max(16);
                for output_line in view::expand_output(&tool.tool_name, output)
                    .into_iter()
                    .take(4)
                {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "    {continuation} │ {}",
                            view::truncate_line(&output_line, output_width)
                        ),
                        t.error,
                    )));
                }
            }
        }
    }
    lines
}

/// The command palette: filtered rows with the selection highlighted,
/// windowed to the available height.
fn palette_lines(app: &App, height: usize, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let query = app.palette_query().unwrap_or("");
    let filtered = filter_commands(query);
    let mut lines = Vec::new();

    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matching commands · esc to close",
            t.dim,
        )));
        return lines;
    }

    let selected = app.palette_index.min(filtered.len() - 1);
    let rows = height.saturating_sub(2).max(1);
    let first = selected.saturating_sub(rows.saturating_sub(1));
    let window: Vec<_> = filtered.iter().enumerate().skip(first).take(rows).collect();
    let range = format!(
        "{}-{}",
        first + 1,
        (first + window.len()).min(filtered.len())
    );
    let header_left = format!("  Results {} · type to filter", filtered.len());
    let pad = width
        .saturating_sub(header_left.chars().count() + range.chars().count() + 2)
        .max(1);
    lines.push(Line::from(Span::styled(
        format!("{header_left}{}{range}", " ".repeat(pad)),
        t.dim,
    )));
    lines.push(Line::from(""));

    for (index, spec) in window {
        let is_selected = index == selected;
        let name = format!("/{}", spec.name);
        let left = format!("  {name:<9} {}", spec.description);
        let pad = width
            .saturating_sub(left.chars().count() + spec.category.chars().count() + 2)
            .max(1);
        let (name_style, desc_style) = if is_selected {
            (t.strong, Style::default())
        } else {
            (t.dim, t.dim)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {name:<9} "), name_style),
            Span::styled(spec.description.to_string(), desc_style),
            Span::styled(format!("{}{}", " ".repeat(pad), spec.category), t.dim),
        ]));
    }
    lines
}

/// Re-render a recorded transcript into the UI: user turns carry the
/// spine, assistant text lands as markdown, and tool activity collapses
/// to the dim call/result summaries. The live activity rail is not
/// reconstructed — replay is a readable history, not a re-run.
fn replay_transcript(app: &mut App, messages: &[orca_harness_core::Message], width: usize) {
    use orca_harness_core::Message;
    for message in messages {
        match message {
            Message::System { .. } => {}
            Message::User { content } => {
                app.push_line(Line::from(""));
                app.push_wrapped(content, "┃ ", theme().strong, width);
                app.turn_count += 1;
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                if let Some(text) = content {
                    if !text.trim().is_empty() {
                        app.push_markdown_block(text, width, BlockSpacing::Section);
                    }
                }
                for call in tool_calls {
                    let line = format!("• {}", view::tool_call_line(&call.name, &call.arguments));
                    app.push_line(Line::from(Span::styled(
                        view::truncate_line(&line, width),
                        theme().dim,
                    )));
                }
            }
            Message::Tool { results } => {
                for result in results {
                    let line = format!(
                        "  {}",
                        view::tool_result_summary(
                            &result.tool_name,
                            &result.output,
                            result.is_error
                        )
                    );
                    app.push_line(Line::from(Span::styled(
                        view::truncate_line(&line, width),
                        theme().dim,
                    )));
                }
            }
        }
    }
    // Replay is history, not a live model phase: the next real turn
    // starts with a clean vertical rhythm.
    app.assistant_started = false;
}

/// Reset every piece of per-conversation UI state. Used by /clear and
/// when the worker adopts another session.
fn reset_conversation_ui(app: &mut App) {
    app.transcript.clear();
    app.pending_history.clear();
    app.scroll = 0;
    app.transcript_max_scroll = 0;
    app.split_inspector_cache = None;
    app.prompt_queue.clear();
    app.tokens_in = 0;
    app.tokens_out = 0;
    app.cache_read_total = 0;
    app.cache_write_total = 0;
    app.usage_steps = 0;
    app.context_tokens = 0;
    app.tool_log.clear();
    app.work_log.clear();
    app.turn_count = 0;
    app.reset_activity();
    app.split_snapshot = None;
}

/// Compact "how long ago" label for the /sessions listing.
fn age_label(created_at: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let delta = now.saturating_sub(created_at);
    match delta {
        0..=59 => format!("{delta}s ago"),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86_399 => format!("{}h ago", delta / 3600),
        _ => format!("{}d ago", delta / 86_400),
    }
}

fn model_picker_lines(picker: &ModelPicker, height: usize, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let filtered = picker.filtered();
    let mut lines = Vec::new();

    let filter_note = if picker.filter.is_empty() {
        "type to filter".to_string()
    } else {
        format!("filter: {}", picker.filter)
    };
    let header_left = format!(
        "  Models {} · {filter_note} · enter switch · esc close",
        filtered.len()
    );
    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(header_left, t.dim)));
        lines.push(Line::from(Span::styled(
            "  no models match — backspace to widen",
            t.dim,
        )));
        return lines;
    }

    let selected = picker.index.min(filtered.len() - 1);
    let rows = height.saturating_sub(2).max(1);
    let first = selected.saturating_sub(rows.saturating_sub(1));
    let window: Vec<_> = filtered.iter().enumerate().skip(first).take(rows).collect();
    let range = format!(
        "{}-{}",
        first + 1,
        (first + window.len()).min(filtered.len())
    );
    let pad = width
        .saturating_sub(header_left.chars().count() + range.chars().count() + 2)
        .max(1);
    lines.push(Line::from(Span::styled(
        format!("{header_left}{}{range}", " ".repeat(pad)),
        t.dim,
    )));
    lines.push(Line::from(""));

    for (index, model) in window {
        let is_selected = index == selected;
        let marker = if is_selected { "▸ " } else { "  " };
        let style = if is_selected { t.strong } else { t.dim };
        let text = format!("  {marker}{}", model.summary());
        lines.push(Line::from(Span::styled(
            view::truncate_line(&text, width),
            style,
        )));
    }
    lines
}

/// Compact token count for the status line: 950, 1.2k, 41.9k, 1.0m.
fn fmt_tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}m", n as f64 / 1_000_000.0),
    }
}

/// The status-line context segment: percentage of the model's window when
/// known (`ctx 33%`), a plain count only when no window is discoverable.
/// Exact figures live behind /usage, never here.
fn context_segment(tokens: u64, window: Option<u64>) -> String {
    match window {
        Some(window) if window > 0 => {
            format!("ctx {}%", (100 * tokens / window).min(999))
        }
        _ => format!("ctx ~{}", fmt_tokens(tokens)),
    }
}

fn provider_lines(picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    let rows = Provider::ALL.iter().map(|provider| {
        let key_note = match provider.key_env() {
            None => "no key needed".to_string(),
            Some(env) if provider.env_key().is_some() => format!("key from ${env}"),
            Some(env) => format!("${env} not set — will ask"),
        };
        format!(
            "{:<12} {:<36} {key_note}",
            provider.label(),
            provider.base_url()
        )
    });
    picker.lines(
        "Select provider · ↑↓ navigate · enter use · esc close",
        rows,
        width,
    )
}

/// The /usage panel: same tray styling as the provider and theme
/// pickers, but read-only — nothing to select, esc/enter/q closes.
fn usage_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let total = app.tokens_in + app.tokens_out + app.cache_read_total + app.cache_write_total;
    let context = match app.context_window {
        Some(window) if window > 0 => format!(
            "{} / {} ({}%)",
            app.context_tokens,
            window,
            (100 * app.context_tokens / window).min(999),
        ),
        _ => format!("~{} (window unknown)", app.context_tokens),
    };
    let mut lines = vec![
        Line::from(Span::styled("  Session usage · esc close", t.dim)),
        Line::from(""),
    ];
    let rows = [
        ("model", app.cfg.model_name.clone()),
        ("context", context),
        ("input", app.tokens_in.to_string()),
        ("output", app.tokens_out.to_string()),
        ("cache read", app.cache_read_total.to_string()),
        ("cache write", app.cache_write_total.to_string()),
        ("total", total.to_string()),
        ("model steps", app.usage_steps.to_string()),
    ];
    for (label, value) in rows {
        let text = format!("  {label:<12} {value}");
        lines.push(Line::from(Span::styled(
            view::truncate_line(&text, width),
            t.dim,
        )));
    }
    lines
}

fn theme_picker_lines(picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    let current = view::theme_name();
    let rows = view::ThemeName::ALL.iter().map(|name| {
        let note = if *name == current { "current" } else { "" };
        format!("{:<16} {:<16} {note}", name.label(), name.slug())
    });
    picker.lines(
        "Select theme · ↑↓ navigate · enter use · esc close",
        rows,
        width,
    )
}

fn view_picker_lines(current: ViewMode, picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    let rows = ViewMode::ALL.iter().map(|mode| {
        let note = if *mode == current { "current" } else { "" };
        let description = match mode {
            ViewMode::Classic => "transcript with inline work rail",
            ViewMode::Split => "tool rail with connected inspector",
        };
        format!("{:<10} {:<38} {note}", mode.label(), description)
    });
    picker.lines(
        "Select view · ↑↓ navigate · enter use · esc close",
        rows,
        width,
    )
}

/// The /settings tray: current values for the persisted preferences,
/// enter drills into the matching picker.
fn settings_lines(app: &App, picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let provider = app.cfg.provider;
    let key_status = match provider.key_env() {
        None => "not needed".to_string(),
        Some(env) if provider.env_key().is_some() => format!("from ${env}"),
        Some(_) if provider.stored_key().is_some() => "saved in config".to_string(),
        Some(_) => "not set".to_string(),
    };
    let approvals = crate::config::stored_approvals(&app.cfg.workspace_root);
    let approvals_status = if approvals.is_empty() {
        "none saved".to_string()
    } else {
        approvals.join(", ")
    };
    let rows = [
        ("provider", provider.label().to_string()),
        ("model", app.cfg.model_name.clone()),
        ("theme", view::theme_name().label().to_string()),
        ("view", app.view_mode.label().to_string()),
        ("api key", key_status),
        ("approvals", approvals_status),
    ];
    let mut lines = picker.lines(
        "Settings · ↑↓ navigate · enter change · esc close",
        rows.iter()
            .map(|(name, value)| format!("{name:<10} {value}")),
        width,
    );
    if let Some(path) = crate::config::config_path() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            view::truncate_line(&format!("  saved to {}", path.display()), width),
            t.dim,
        )));
    }
    lines
}

/// This workspace's saved always-allowed tools; enter revokes the
/// selected one so it prompts again.
fn approvals_lines(tools: &[String], picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    picker.lines(
        "Saved approvals (this workspace) · ↑↓ navigate · enter revoke · esc close",
        tools.iter().cloned(),
        width,
    )
}

/// The harness extension catalog with live on/off state; enter toggles
/// the selected extension and the list stays open.
fn extensions_picker_lines(picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    let rows = crate::extensions::EXTENSIONS.iter().map(|spec| {
        let state = if crate::extensions::is_enabled(spec) {
            "on "
        } else {
            "off"
        };
        format!("{:<12} {state}  {}", spec.name, spec.description)
    });
    picker.lines(
        "Extensions · ↑↓ navigate · enter toggle · esc close",
        rows,
        width,
    )
}

/// Recorded sessions for this workspace, newest first; enter resumes
/// the selected one. Same interface as /provider and /theme.
fn sessions_picker_lines(
    sessions: &[orca_harness_extensions::SessionFile],
    current: Option<&str>,
    picker: &ListPicker,
    width: usize,
) -> Vec<Line<'static>> {
    let rows = sessions.iter().map(|session| {
        let note = if current == Some(session.meta.id.as_str()) {
            "  (current)"
        } else {
            ""
        };
        format!(
            "{}  {:<8} {}{note}",
            session.meta.id,
            age_label(session.meta.created_at),
            session.meta.model,
        )
    });
    picker.lines(
        "Sessions (this workspace) · ↑↓ navigate · enter resume · esc close",
        rows,
        width,
    )
}

fn api_key_lines(provider: Provider, input: &str) -> Vec<Line<'static>> {
    let t = theme();
    vec![
        Line::from(Span::styled(
            format!(
                "  {} API key (saved for future sessions) · enter confirm · esc cancel",
                provider.label()
            ),
            t.warn,
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  key: ", t.dim),
            Span::raw("•".repeat(input.chars().count())),
        ]),
    ]
}

fn byte_index(s: &str, char_index: usize) -> usize {
    s.char_indices()
        .nth(char_index)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::backend::TestBackend;

    fn test_app() -> App {
        let mut app = App::new(TuiConfig {
            model_name: "test".into(),
            workspace_name: "/workspace".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
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
    fn empty_session_has_a_useful_static_welcome() {
        let mut app = App::new(TuiConfig {
            model_name: "gpt-oss:20b".into(),
            workspace_name: "/workspace/orca-harness".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
        });
        let screen = rendered_rows(&mut app, 90, 30).join("\n");

        assert!(screen.contains("orcacode"), "title missing: {screen}");
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
    fn status_line_starts_with_model_and_ends_with_workspace_name() {
        let mut app = App::new(TuiConfig {
            model_name: "gpt-oss:20b".into(),
            workspace_name: "/workspace/orca-harness".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
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
            inspector.contains("○ SHELL"),
            "tool identity repeats: {inspector}"
        );
        assert!(
            inspector.contains("cargo test"),
            "input is expanded: {inspector}"
        );
        assert!(inspector.contains("Waiting for result"));

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
        })
    }

    /// One test, single-threaded, because the theme is process-global: it
    /// must not interleave with any other mutating test. Ends by restoring
    /// the default so later tests see a clean state.
    #[test]
    fn theme_command_switches_and_rejects() {
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
