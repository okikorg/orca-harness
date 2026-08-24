// Key and mouse dispatch for the modal overlays, approval prompts, and
// the slash palette. The big one is [`handle_overlay_key`]: every picker
// row, toggle, and drill-down the overlays support funnels through the
// `After` state machine so side effects (worker sends, config writes,
// overlay replacement) happen only after the current borrow ends.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

use crate::msg::{ApprovalResponse, Provider, ProviderAuth, WorkerCmd};
use crate::tui::command_catalog::{filter_commands, CommandSpec};
use crate::tui::components::picker::{ListPicker, PickerEvent};
use crate::tui::components::transcript::{
    set_transcript_spacing, transcript_spacing, TranscriptSpacing,
};
use crate::view;

use super::format::byte_index;
use super::render::matching_indices;
use super::state::{App, LocationEntry, Overlay, ViewMode};
use super::{clear_tool_connectors, push_error, push_notice, remove_skill, PICKER_ROWS};

mod effects;
mod helpers;
mod overlays;

pub(crate) use helpers::*;
pub(crate) use overlays::*;
