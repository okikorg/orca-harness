// The interactive terminal. Fullscreen alternate-screen app: the
// transcript fills the window from the top, a live region (streaming
// tail, approval prompts, the slash palette) sits above the composer,
// and the composer plus status line are pinned to the bottom. The wheel,
// shift+↑/↓, and PgUp/PgDn all scroll the in-app transcript buffer.
//
// Mouse capture stays on: a terminal forwards the wheel to a fullscreen
// app only under capture. Selection is not lost to it — terminals reserve
// a modifier drag for their own selection while an app holds the mouse
// (option-drag on macOS terminals, shift-drag elsewhere) — and `/copy`
// (ctrl+y) writes to the clipboard through the terminal, reaching content
// that has scrolled past, which no drag can.
mod app;
mod clipboard;
mod clipboard_image;
mod command_catalog;
mod commands;
pub(crate) mod components;
mod composer;
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
mod subagents;
mod text;

pub(crate) use self::text::{clear_tool_connectors, line_text, push_wrapped_lines, replace_block};
pub(crate) use crate::prompt::strip_location_mentions;

pub(crate) use self::state::{
    App, EffortPicker, InspectorBodyCache, InspectorMode, LocationPicker, ModelPicker, Overlay,
    RunState, SkillMentionPicker, ToolActivity, TuiConfig, ViewMode,
};
#[cfg(test)]
pub(crate) use self::state::{LocationEntry, ToolRecord, SESSION_ACTIONS, SETTINGS_ROWS};

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

#[cfg(test)]
use self::command_catalog::filter_commands;
#[cfg(test)]
use crate::msg::ApprovalResponse;
#[cfg(test)]
use crate::msg::Provider;
use crate::msg::{UiMsg, WorkerCmd};
use crate::tui::components::notification::Notification;
#[cfg(test)]
use crate::tui::components::picker::ListPicker;
#[cfg(test)]
use crate::tui::components::transcript::line_is_blank;
#[cfg(test)]
use crate::tui::components::transcript::BlockSpacing;
#[cfg(test)]
use crate::tui::components::transcript::{
    set_transcript_spacing, transcript_spacing, TranscriptSpacing,
};
use crate::view::{self, theme};

use self::format::elapsed_label;
#[cfg(test)]
use self::format::redact_command;
use self::input::{expand_pastes, prompt_images};

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

mod runtime;

pub use runtime::run;
pub(super) use runtime::*;

#[cfg(test)]
include!("inner_tests.rs");
