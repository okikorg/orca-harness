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
mod app;
mod commands;
mod events;
mod format;
mod input;
mod inspector;
mod keys;

pub(crate) use self::commands::{remove_skill, slash_command};

pub(crate) use self::events::{flush_terminal_requests, handle_terminal_event, handle_ui_msg};
#[cfg(test)]
pub(crate) use self::events::{handle_harness_event, handle_subagent_event};
pub(crate) use self::keys::{
    handle_approval_key, handle_overlay_key, history_nav, palette_move, palette_selection,
};
mod render;
mod state;
mod text;

pub(crate) use self::render::strip_location_mentions;
pub(crate) use self::text::{clear_tool_connectors, line_text, push_wrapped_lines, replace_block};

pub(crate) use self::state::{
    App, InspectorBodyCache, LocationEntry, LocationPicker, ModelPicker, Overlay, RunState,
    ToolActivity, TuiConfig, ViewMode,
};
#[cfg(test)]
pub(crate) use self::state::{ToolRecord, SESSION_ACTIONS, SETTINGS_ROWS};

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    EventStream as CtEventStream,
};
#[cfg(test)]
use crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::text::{Line, Span};
use ratatui::Terminal;
use tokio::sync::mpsc;

use orca_harness_core::CancellationToken;
#[cfg(test)]
use orca_harness_extensions::HarnessEvent;

use crate::clipboard;
#[cfg(test)]
use crate::commands::filter_commands;
use crate::components::notification::Notification;
#[cfg(test)]
use crate::components::picker::ListPicker;
#[cfg(test)]
use crate::components::transcript::line_is_blank;
#[cfg(test)]
use crate::components::transcript::BlockSpacing;
#[cfg(test)]
use crate::components::transcript::{
    set_transcript_spacing, transcript_spacing, TranscriptSpacing,
};
#[cfg(test)]
use crate::msg::ApprovalResponse;
#[cfg(test)]
use crate::msg::Provider;
use crate::msg::{UiMsg, WorkerCmd};
use crate::view::{self, theme};

use self::format::elapsed_label;
#[cfg(test)]
use self::format::redact_command;
use self::input::expand_pastes;

use self::inspector::inspector_text_content;
#[cfg(test)]
use self::inspector::{
    inspector_code_facts, inspector_output_preview, shallow_json_preview, tool_inspector_lines,
};
#[cfg(test)]
use self::render::activity_lines_selected;
#[cfg(test)]
use self::render::reset_conversation_ui;
#[cfg(test)]
use self::render::{
    activity_lines, context_segment, live_lines, mode_segment, palette_lines, projected_transcript,
    stabilize_transcript_scroll, stats_segments, todo_segment,
};
use self::render::{draw, transcript_content_width};
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
pub(super) const PICKER_ROWS: usize = 10;
/// How many sessions the /sessions picker displays at once (newest
/// first); the cursor pages through the rest like the /models picker.
const SESSIONS_WINDOW: usize = 5;
const LIVE_TOOL_ROWS: usize = 8;
const QUEUE_PREVIEW_ROWS: usize = 3;

/// The active theme is process-global, so the test that switches it and
/// the tests that assert theme-derived colors must not interleave. Both
/// take this lock; without it they race whenever the harness happens to
/// The active theme is process-global, so the test that switches it and
/// the tests that assert theme-derived colors must not interleave. Both
/// take this lock; without it they race whenever the harness happens to
/// schedule them together.
#[cfg(test)]
static THEME_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The transcript-spacing preference is process-global and seeded by
/// `App::new` from the config file. Tests that construct an `App` while
/// another test is mid-transition on the spacing picker race on that
/// static, so both sides of the transition take this lock.
static SPACING_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
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

/// A compact transcript notification: the dot keeps system status scannable
/// without giving it the visual weight of transcript content.
fn push_notice(app: &mut App, text: impl Into<String>) {
    app.push_line(Notification::notice(text).line());
}

/// [`push_notice`] — a failed command is still the system talking, and a
/// line without the glyph reads as model output — with the body in the
/// error color so severity and origin are two separate cues.
fn push_error(app: &mut App, text: impl Into<String>) {
    app.push_line(Notification::error(text).line());
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
    app.push_user_prompt(&prompt, width);
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
    app.push_user_prompt(&prompt, width);
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

#[cfg(test)]
// Kept separate so the runtime module remains navigable.
include!("inner_tests.rs");
