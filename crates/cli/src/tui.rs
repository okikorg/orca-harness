//! The interactive terminal. Fullscreen alternate-screen app: the
//! transcript fills the window from the top, a live region (streaming
//! tail, approval prompts, the slash palette) sits above the composer,
//! and the composer plus status line are pinned to the bottom. PgUp/PgDn
//! scroll the in-app transcript buffer.

use std::io;
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
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;

use orca_harness_core::CancellationToken;
use orca_harness_extensions::HarnessEvent;
use orca_harness_model_openrouter::ModelInfo;

use crate::commands::{filter_commands, CommandSpec};
use crate::msg::{ApprovalRequest, ApprovalResponse, Provider, UiMsg, WorkerCmd};
use crate::view::{self, theme};

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const EXPAND_MAX_LINES: usize = 200;
const TRANSCRIPT_CAP: usize = 5000;
const SCROLL_PAGE: usize = 10;
const PALETTE_ROWS: usize = 8;
const PICKER_ROWS: usize = 10;
const LIVE_TOOL_ROWS: usize = 8;
pub struct TuiConfig {
    pub model_name: String,
    pub workspace_name: String,
    /// Shared handle behind the `subagent` tool's nesting cap;
    /// `/subagents` adjusts it live.
    pub subagent_depth: orca_harness_tools::SubagentDepth,
    /// Live background-work counters rendered in the status line.
    pub stats: orca_harness_tools::BackgroundStats,
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
    fn selected_id(&self) -> Option<String> {
        let filtered = self.filtered();
        filtered
            .get(self.index.min(filtered.len().saturating_sub(1)))
            .map(|m| m.id.clone())
    }
}

/// A modal selector rendered in the live region. Approval prompts win
/// over overlays; overlays win over the slash palette.
enum Overlay {
    /// Model selector over the fetched catalog.
    Models(ModelPicker),
    /// Provider selector (openrouter, openai, local).
    Providers { index: usize },
    /// Masked API-key entry for a provider whose key is not in the env.
    ApiKey { provider: Provider, input: String },
}

/// A finished tool call kept around so the user can expand its full
/// output later with `/expand n`.
struct ToolRecord {
    call_line: String,
    tool_name: String,
    output: serde_json::Value,
}

/// A completed turn's compacted work rail. The transcript keeps only the
/// summary until the user asks to inspect the tree with ctrl+o.
struct CompletedWork {
    turn: usize,
    summary: String,
    lines: Vec<Line<'static>>,
    expanded: bool,
}

struct ThinkingRecord {
    elapsed: Duration,
}

struct ToolActivity {
    call_line: String,
    tool_name: String,
    input: serde_json::Value,
    started: Instant,
    elapsed: Option<Duration>,
    output: Option<serde_json::Value>,
    is_error: bool,
    approval: Option<String>,
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
    prompt_history: Vec<String>,
    history_pos: Option<usize>,
    tokens_in: u64,
    tokens_out: u64,
    spinner_frame: usize,
    quit: bool,
    tool_log: Vec<ToolRecord>,
    /// Completed per-turn work trees, newest last.
    work_log: Vec<CompletedWork>,
    /// Number of user turns rendered in this session.
    turn_count: usize,
    /// Call lines for in-flight tool calls, keyed by call id.
    pending_calls: std::collections::HashMap<String, usize>,
    activity_tools: Vec<ToolActivity>,
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
            prompt_history: Vec::new(),
            history_pos: None,
            tokens_in: 0,
            tokens_out: 0,
            spinner_frame: 0,
            quit: false,
            tool_log: Vec::new(),
            work_log: Vec::new(),
            turn_count: 0,
            pending_calls: std::collections::HashMap::new(),
            activity_tools: Vec::new(),
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
        let body_width = width.saturating_sub(indent.len()).max(16);
        for paragraph in text.split('\n') {
            if paragraph.trim().is_empty() {
                self.push_line(Line::from(""));
                continue;
            }
            for piece in textwrap::wrap(paragraph, body_width) {
                self.push_line(Line::from(Span::styled(format!("{indent}{piece}"), style)));
            }
        }
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
        });
    }

    fn reset_activity(&mut self) {
        self.reasoning.clear();
        self.reasoning_started = None;
        self.thinking_log.clear();
        self.activity_tools.clear();
        self.pending_calls.clear();
        self.pending_assistant = None;
        self.assistant_started = false;
    }

    fn ensure_assistant_started(&mut self) {
        if self.assistant_started {
            return;
        }
        self.push_line(Line::from(""));
        self.assistant_started = true;
    }

    /// Start another assistant-owned block with exactly one row of breathing
    /// room, whether it is the first answer or follows earlier work.
    fn begin_assistant_block(&mut self) {
        if self.assistant_started {
            self.push_line(Line::from(""));
        } else {
            self.ensure_assistant_started();
        }
    }

    fn commit_activity(&mut self, width: usize) {
        self.flush_reasoning();
        let lines = activity_lines(self, width, false);
        if !lines.is_empty() {
            self.begin_assistant_block();
            let summary = collapsed_activity_line(self);
            let summary_text = line_text(&summary);
            self.push_line(summary);
            self.work_log.push(CompletedWork {
                turn: self.turn_count,
                summary: summary_text,
                lines,
                expanded: false,
            });
            if self.work_log.len() > 100 {
                self.work_log.remove(0);
            }
        }
        self.thinking_log.clear();
        self.activity_tools.clear();
        self.pending_calls.clear();
    }

    /// Move pending lines into the transcript. A reader who has scrolled
    /// up stays anchored; the bottom follows new content otherwise.
    fn absorb_pending(&mut self) {
        if self.pending_history.is_empty() {
            return;
        }
        let added = self.pending_history.len();
        self.transcript.append(&mut self.pending_history);
        if self.scroll > 0 {
            self.scroll += added;
        }
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
                    Some(msg) => handle_ui_msg(&mut app, msg, width),
                    None => app.quit = true,
                }
            }
            _ = ticker.tick(), if app.running() => {
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
        "orca · session ended · tokens in {} out {}",
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
            match mouse.kind {
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
                expand_tool(app, 1, width);
            }
        }
        KeyCode::PageUp => app.scroll += SCROLL_PAGE,
        KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(SCROLL_PAGE),
        KeyCode::Esc => {
            if app.palette_query().is_some() {
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
            submit(app, worker, width);
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
        CloseAndSend(WorkerCmd),
    }
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
                KeyCode::Enter => match picker.selected_id() {
                    Some(id) => After::CloseAndSend(WorkerCmd::SetModel { id }),
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
        Overlay::Providers { index } => match key.code {
            KeyCode::Up => {
                *index = index.saturating_sub(1);
                After::Nothing
            }
            KeyCode::Down => {
                *index = (*index + 1).min(Provider::ALL.len() - 1);
                After::Nothing
            }
            KeyCode::Enter => {
                let provider = Provider::ALL[(*index).min(Provider::ALL.len() - 1)];
                if provider.key_env().is_some() && provider.env_key().is_none() {
                    // No key in the shell: ask for one before switching.
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
        Overlay::ApiKey { provider, input } => match key.code {
            KeyCode::Enter => {
                let key = input.trim().to_string();
                if key.is_empty() {
                    After::Nothing
                } else {
                    After::CloseAndSend(WorkerCmd::SetProvider {
                        provider: *provider,
                        api_key: Some(key),
                    })
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
    };
    match after {
        After::Nothing => {}
        After::Close => app.overlay = None,
        After::Replace(next) => app.overlay = Some(next),
        After::CloseAndSend(cmd) => {
            app.overlay = None;
            send_or_report(app, worker, cmd);
        }
    }
}

fn send_or_report(app: &mut App, worker: &mpsc::UnboundedSender<WorkerCmd>, cmd: WorkerCmd) {
    if worker.send(cmd).is_err() {
        app.push_line(Line::from(Span::styled(
            "worker is gone; restart orca",
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
        KeyCode::Char('a') | KeyCode::Char('A') => Some(ApprovalResponse::AllowAlways),
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
                ApprovalResponse::Deny => "denied",
            };
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
        return;
    }
    if app.running() {
        return; // one run at a time; Esc cancels
    }
    app.composer.clear();
    app.cursor = 0;
    app.history_pos = None;
    app.prompt_history.push(prompt.clone());

    if let Some(command) = prompt.strip_prefix('/') {
        slash_command(app, command.trim(), worker, width);
        return;
    }

    app.reset_activity();

    if app.turn_count > 0 {
        app.push_line(Line::from(""));
    }
    app.push_wrapped(&prompt, "┃ ", theme().strong, width);
    app.turn_count += 1;
    let cancel = CancellationToken::new();
    if worker
        .send(WorkerCmd::Run {
            prompt,
            cancel: cancel.clone(),
        })
        .is_err()
    {
        app.push_line(Line::from(Span::styled(
            "worker is gone; restart orca",
            theme().error,
        )));
        return;
    }
    app.run = RunState::Running {
        started: Instant::now(),
        cancel,
    };
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

    let summary = work.summary.clone();
    let lines = work.lines.clone();
    let inserted = replace_line(&mut app.pending_history, &summary, &lines)
        || replace_line(&mut app.transcript, &summary, &lines);
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

fn replace_line(
    lines: &mut Vec<Line<'static>>,
    target: &str,
    replacement: &[Line<'static>],
) -> bool {
    let Some(index) = lines.iter().rposition(|line| line_text(line) == target) else {
        return false;
    };
    lines.splice(index..=index, replacement.iter().cloned());
    true
}

fn slash_command(
    app: &mut App,
    command: &str,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    width: usize,
) {
    let dim = theme().dim;
    if let Some(rest) = command.strip_prefix("expand") {
        let nth = rest.trim().parse::<usize>().unwrap_or(1).max(1);
        expand_tool(app, nth, width);
        return;
    }
    // "models" before "model": both take arguments, and the bare match
    // below only handles the argument-less forms.
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
                    "worker is gone; restart orca",
                    theme().error,
                )));
            } else {
                app.push_line(Line::from(Span::styled("fetching models…", dim)));
            }
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("model ") {
        let id = rest.trim().to_string();
        if !id.is_empty() {
            if worker.send(WorkerCmd::SetModel { id }).is_err() {
                app.push_line(Line::from(Span::styled(
                    "worker is gone; restart orca",
                    theme().error,
                )));
            }
            return;
        }
    }
    match command {
        "quit" | "exit" | "q" => app.quit = true,
        "clear" => {
            let _ = worker.send(WorkerCmd::Clear);
            app.transcript.clear();
            app.pending_history.clear();
            app.scroll = 0;
            app.tokens_in = 0;
            app.tokens_out = 0;
            app.tool_log.clear();
            app.work_log.clear();
            app.turn_count = 0;
            app.reset_activity();
        }
        "model" => {
            let text = format!("model: {}", app.cfg.model_name);
            app.push_line(Line::from(Span::styled(text, dim)));
        }
        "provider" => {
            app.overlay = Some(Overlay::Providers { index: 0 });
        }
        "help" | "" => {
            for entry in [
                "/help        show this help",
                "/expand [n]  full output of the n-th latest tool call (1 = latest)",
                "/clear       reset the conversation context",
                "/model [id]  show the current model, or switch to another",
                "/models [f]  pick a model from the endpoint's catalog",
                "/provider    switch provider (openrouter, openai, local)",
                "/subagents [n] show or set subagent nesting depth (1-5)",
                "/quit        exit",
                "keys: enter send · esc cancel run · ctrl+o reveal latest work tree",
                "      pgup/pgdn scroll · ctrl+c quit · up/down history",
                "approvals: y allow once · a always allow tool · n deny",
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

fn handle_ui_msg(app: &mut App, msg: UiMsg, width: usize) {
    match msg {
        UiMsg::Event(event) => handle_harness_event(app, event, width),
        UiMsg::SubagentEvent { .. } => {}
        UiMsg::Approval(request) => app.approval = Some(request),
        UiMsg::Models(result) => {
            let t = theme();
            let seed = app.picker_pending.take().unwrap_or_default();
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
        UiMsg::ProviderChanged { provider, model } => {
            app.cfg.model_name = model.clone();
            app.push_line(Line::from(Span::styled(
                format!("provider: {provider} · model: {model}"),
                theme().dim,
            )));
        }
        UiMsg::RunDone(result) => {
            // Flush any partial stream (interrupted mid-generation).
            if result.is_err() {
                app.commit_activity(width);
                let partial = if !app.text.trim().is_empty() {
                    Some(std::mem::take(&mut app.text))
                } else {
                    app.pending_assistant.take()
                };
                if let Some(partial) = partial {
                    app.begin_assistant_block();
                    for line in view::markdown_lines(&partial, width, "  ") {
                        app.push_line(line);
                    }
                }
            }
            app.text.clear();
            app.run = RunState::Idle;
            app.approval = None;
            if let Err(err) = result {
                app.ensure_assistant_started();
                let (style, label) = if err.to_lowercase().contains("cancel") {
                    (theme().dim, "interrupted".to_string())
                } else {
                    (theme().error, format!("run failed: {err}"))
                };
                app.push_wrapped(&label, "  ", style, width);
            }
        }
    }
}

fn handle_harness_event(app: &mut App, event: HarnessEvent, width: usize) {
    match event {
        HarnessEvent::AssistantDelta { text } => app.text.push_str(&text),
        HarnessEvent::ReasoningDelta { text } => {
            if app.reasoning.is_empty() && !text.is_empty() {
                app.reasoning_started = Some(Instant::now());
            }
            app.reasoning.push_str(&text);
        }
        HarnessEvent::Assistant { message } => {
            app.flush_reasoning();
            app.text.clear();
            app.pending_assistant = Some(message);
        }
        HarnessEvent::ToolCall {
            tool_call_id,
            tool_name,
            input,
        } => {
            app.flush_reasoning();
            if let Some(message) = app.pending_assistant.take() {
                if !message.trim().is_empty() {
                    app.begin_assistant_block();
                    for line in view::markdown_lines(&message, width, "  ") {
                        app.push_line(line);
                    }
                }
            }
            let call_line = view::tool_call_line(&tool_name, &input);
            let index = app.activity_tools.len();
            app.activity_tools.push(ToolActivity {
                call_line,
                tool_name,
                input,
                started: Instant::now(),
                elapsed: None,
                output: None,
                is_error: false,
                approval: None,
            });
            app.pending_calls.insert(tool_call_id, index);
        }
        HarnessEvent::ToolResult {
            tool_call_id,
            tool_name,
            output,
            is_error,
        } => {
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
            app.push_record(ToolRecord {
                call_line,
                tool_name,
                output,
            });
        }
        HarnessEvent::Usage { usage } => {
            app.tokens_in += usage.input_tokens;
            app.tokens_out += usage.output_tokens;
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
                app.begin_assistant_block();
                for line in view::markdown_lines(&answer, width, "  ") {
                    app.push_line(line);
                }
            }
        }
        HarnessEvent::AgentStart | HarnessEvent::Error { .. } => {}
    }
}

fn welcome_lines(height: usize, width: usize, cfg: &TuiConfig) -> Vec<Line<'static>> {
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
            Span::styled("orca", t.strong),
            Span::styled(" harness", t.dim),
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
    let top = height.saturating_sub(content.len()) / 2;
    std::iter::repeat_n(Line::from(""), top)
        .chain(content)
        .collect()
}

fn draw(frame: &mut Frame, app: &mut App) {
    let width = frame.area().width as usize;
    let live = live_lines(app, width);
    let live_height = live.len().min(PALETTE_ROWS + 7) as u16;
    let [transcript_area, live_area, _composer_gap_area, composer_area, status_area] =
        Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(live_height),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());

    // Transcript: committed history plus a render-only projection of the
    // in-progress turn. Deltas therefore appear in their final location
    // instead of streaming through the temporary area and jumping here.
    let height = transcript_area.height as usize;
    let projected = projected_transcript(app, width);
    app.scroll = app.scroll.min(projected.len().saturating_sub(height));
    let visible = if projected.is_empty() && !app.running() {
        welcome_lines(height, width, &app.cfg)
    } else {
        let (start, end) = view::scroll_window(projected.len(), height, app.scroll);
        projected[start..end].to_vec()
    };
    frame.render_widget(Paragraph::new(Text::from(visible)), transcript_area);

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
    let composer_line = if app.composer.is_empty() && !app.running() {
        Line::from(vec![
            Span::styled("│ ", theme().accent),
            Span::styled("ask anything · /help for commands", theme().dim),
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
    } else {
        "idle"
    };
    let hint = if app.scroll > 0 {
        "scrolled · pgdn to follow"
    } else if app.overlay.is_some() && app.approval.is_none() {
        "↑↓ navigate · enter use · esc close"
    } else if app.palette_query().is_some() && app.approval.is_none() {
        "↑↓ navigate · enter use · tab complete · esc close"
    } else if app.running() {
        "esc interrupt"
    } else {
        "enter send · ctrl+o expand · pgup scroll"
    };
    let status = format!(
        " {} · {} · in {} out {} · {}",
        app.cfg.model_name, state, app.tokens_in, app.tokens_out, hint
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            view::truncate_line(&status, width),
            theme().dim,
        ))),
        status_area,
    );
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
                "    [y] allow once   [a] always allow this tool   [n] deny",
                t.dim,
            )),
        ];
    }
    if let Some(overlay) = &app.overlay {
        return match overlay {
            Overlay::Models(picker) => model_picker_lines(picker, PICKER_ROWS + 2, width),
            Overlay::Providers { index } => provider_lines(*index, width),
            Overlay::ApiKey { provider, input } => api_key_lines(*provider, input),
        };
    }
    if app.palette_query().is_some() {
        return palette_lines(app, PALETTE_ROWS + 2, width);
    }
    if app.running() {
        let mut lines = Vec::new();
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
    Vec::new()
}

fn projected_transcript(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines = app.transcript.clone();
    if !app.running() {
        return lines;
    }

    let activity = activity_lines(app, width, true);
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

    if !activity.is_empty() {
        lines.push(Line::from(""));
        lines.extend(activity);
    }
    if let Some(answer) = answer {
        lines.push(Line::from(""));
        lines.extend(view::markdown_lines(answer, width, "  "));
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

/// One quiet line for completed work. The full activity tree is retained in
/// `work_log`; this summary keeps the answer visually dominant.
fn collapsed_activity_line(app: &App) -> Line<'static> {
    let tool_count = app.activity_tools.len();
    let thinking_count = app.thinking_log.len();
    let failed = app
        .activity_tools
        .iter()
        .filter(|tool| tool.is_error)
        .count();
    let elapsed = match &app.run {
        RunState::Running { started, .. } => started.elapsed(),
        RunState::Idle => app
            .activity_tools
            .iter()
            .filter_map(|tool| tool.elapsed)
            .max()
            .unwrap_or_default(),
    };

    let mut parts = Vec::new();
    if tool_count > 0 {
        parts.push(plural(tool_count, "tool"));
    }
    if thinking_count > 0 {
        parts.push(plural(thinking_count, "thinking update"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    parts.push(elapsed_label(elapsed));

    let style = if failed > 0 {
        theme().error
    } else {
        theme().dim
    };
    Line::from(Span::styled(
        format!("  ▸ Work · {}", parts.join(" · ")),
        style,
    ))
}

/// Render the current run as one coherent activity rail. While the run is
/// live this includes the latest reasoning tail and pending tool states;
/// once committed, the rail is retained for on-demand expansion.
fn activity_lines(app: &App, width: usize, live: bool) -> Vec<Line<'static>> {
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
        let marker = if live && current_thinking {
            "▾"
        } else {
            "▸"
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
        format!("  ▾ Work · ✓ {complete} · □ {running}")
    } else {
        format!("  ▾ Work · {}", plural(app.activity_tools.len(), "tool"))
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
        let fixed_width = 12 + detail.chars().count();
        let call = view::truncate_line(
            &tool.call_line,
            row_width.saturating_sub(fixed_width).max(8),
        );
        lines.push(Line::from(vec![
            Span::styled(format!("    {branch} "), t.dim),
            Span::styled(format!("{glyph} "), status_style),
            Span::styled(call, t.accent),
            Span::styled(format!(" · {detail}"), status_style),
        ]));
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

fn provider_lines(selected: usize, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let mut lines = vec![
        Line::from(Span::styled(
            "  Select provider · ↑↓ navigate · enter use · esc close",
            t.dim,
        )),
        Line::from(""),
    ];
    for (index, provider) in Provider::ALL.iter().enumerate() {
        let is_selected = index == selected.min(Provider::ALL.len() - 1);
        let marker = if is_selected { "▸ " } else { "  " };
        let key_note = match provider.key_env() {
            None => "no key needed".to_string(),
            Some(env) if provider.env_key().is_some() => format!("key from ${env}"),
            Some(env) => format!("${env} not set — will ask"),
        };
        let text = format!(
            "  {marker}{:<12} {:<36} {key_note}",
            provider.label(),
            provider.base_url()
        );
        let style = if is_selected { t.strong } else { t.dim };
        lines.push(Line::from(Span::styled(
            view::truncate_line(&text, width),
            style,
        )));
    }
    lines
}

fn api_key_lines(provider: Provider, input: &str) -> Vec<Line<'static>> {
    let t = theme();
    vec![
        Line::from(Span::styled(
            format!(
                "  {} API key (kept for this session only) · enter confirm · esc cancel",
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
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
        });
        // A transcript taller than any viewport so scrolling has room.
        for i in 0..100 {
            app.transcript.push(Line::from(format!("line {i}")));
        }
        app
    }

    fn mouse(kind: MouseEventKind) -> CtEvent {
        CtEvent::Mouse(MouseEvent {
            kind,
            column: 0,
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
        let mut app = test_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ReasoningDelta { text: "hmm".into() },
            80,
        );
        handle_ui_msg(&mut app, UiMsg::RunDone(Err("cancelled".into())), 80);
        let texts = pending_texts(&app);
        assert!(
            texts.iter().any(|t| t.contains("Work · 1 thinking update")),
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
            second < 2 || texts[second - 2] != "",
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
        app.tool_log.push(ToolRecord {
            call_line: "shell $ ls".into(),
            tool_name: "shell".into(),
            output: serde_json::json!({}),
        });

        slash_command(&mut app, "clear", &tx, 80);

        assert!(app.transcript.is_empty(), "transcript wiped");
        assert_eq!(app.scroll, 0);
        assert_eq!(app.tokens_in, 0);
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
    fn completed_run_commits_one_collapsed_activity_rail_before_the_answer() {
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
        assert!(work < answer, "summary precedes answer: {joined}");
        assert!(!joined.contains("shell $ cargo test"));
        assert!(!joined.contains("✓ shell $ cargo test · exit 0 · 42 tests passed"));
        assert!(
            !joined.contains("private reasoning text"),
            "completed thinking is collapsed"
        );

        let details = flat_lines(&app.work_log.last().expect("work tree retained").lines);
        assert!(details.contains("Thinking"));
        assert!(details.contains("shell $ cargo test"));
        assert!(details.contains("✓ shell $ cargo test · exit 0 · 42 tests passed"));

        app.absorb_pending();
        assert!(expand_latest_work(&mut app));
        let expanded = app
            .transcript
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!expanded.contains("▸ Work · 1 tool"));
        assert!(expanded.contains("shell $ cargo test"));
        let tool = expanded.find("shell $ cargo test").expect("expanded tool");
        let answer = expanded.find("Everything passed.").expect("answer");
        assert!(tool < answer, "work expands in place: {expanded}");
        let once = app.transcript.len();
        assert!(expand_latest_work(&mut app));
        assert_eq!(app.transcript.len(), once, "repeat expansion is a no-op");
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
        let mut app = test_app();
        handle_ui_msg(
            &mut app,
            UiMsg::RunDone(Err("model endpoint unavailable".into())),
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
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
        });
        let screen = rendered_rows(&mut app, 90, 30).join("\n");

        assert!(screen.contains("orca harness"), "title missing: {screen}");
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
    fn assistant_deltas_render_in_the_transcript_above_live_status() {
        let mut app = App::new(TuiConfig {
            model_name: "test".into(),
            workspace_name: "/workspace".into(),
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
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
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
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
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
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
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
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
        let mut app = test_app();
        app.picker_pending = Some("acme".into());
        handle_ui_msg(&mut app, UiMsg::Models(Ok(catalog())), 80);
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
        app.overlay = Some(Overlay::Providers { index: 0 });
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
    }

    #[test]
    fn slash_provider_opens_the_provider_overlay() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        slash_command(&mut app, "provider", &tx, 80);
        assert!(matches!(app.overlay, Some(Overlay::Providers { index: 0 })));
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
            subagent_depth: depth,
            stats: orca_harness_tools::BackgroundStats::new(),
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
