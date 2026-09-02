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
use crate::view::glyphs::{set_ui_style, ui_style, UiStyle};

use super::format::byte_index;
use super::render::matching_indices;
use super::state::{
    App, EffortPicker, InspectorMode, LocationEntry, ModelPicker, Overlay, SubagentSetting,
    ViewMode,
};
use super::subagents::{
    apply_subagent_value, subagent_selected, subagent_setting_label, subagent_values,
};
use super::{
    clear_tool_connectors, plugin_picker_action, push_error, push_notice, remove_skill, PICKER_ROWS,
};

mod catalogs;
mod effects;
mod helpers;
mod location_mentions;
mod overlays;
mod skill_mentions;

pub(crate) use helpers::*;
pub(crate) use overlays::*;
