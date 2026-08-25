//! The interactive terminal's persistent state: configuration handed in
//! from the runner, the full-screen [`App`] structure, and the modal
//! selectors (model/location/provider/theme/view/spacing/usage/api-key/
//! settings/approvals/extensions/mcp/skills/sessions) that overlays can
//! host. Rendering and key handling live in the sibling modules; this one
//! only owns the data.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use ratatui::text::Line;

use crate::msg::{ApprovalRequest, Provider};
use crate::tui::components::ask::AskForm;
use crate::tui::components::picker::{ListPicker, PickerAction};
use orca_harness_core::{CancellationToken, Image};
use orca_harness_model_providers::openrouter::ModelInfo;

use super::format::TokenEstimator;
use super::PICKER_ROWS;

/// Everything the terminal needs to know about the workspace it is
/// rendering, passed once by the runner and updated by slash commands
/// and the event loop.
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
pub(crate) struct ModelPicker {
    pub(crate) models: Vec<ModelInfo>,
    pub(crate) filter: String,
    pub(crate) index: usize,
}

/// Workspace-relative file and directory inserted into the composer by
/// the `@` mention picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocationEntry {
    pub(crate) path: String,
    pub(crate) directory: bool,
}

pub(crate) struct LocationPicker {
    pub(crate) entries: Vec<LocationEntry>,
    pub(crate) query: String,
    /// Character offset of the `@` that opened this picker.
    pub(crate) token_start: usize,
    pub(crate) picker: ListPicker,
}

impl LocationPicker {
    pub(crate) fn filtered(&self) -> Vec<&LocationEntry> {
        let needle = self.query.to_lowercase();
        self.entries
            .iter()
            .filter(|entry| needle.is_empty() || entry.path.to_lowercase().contains(&needle))
            .take(PICKER_ROWS)
            .collect()
    }

    pub(crate) fn selected(&self) -> Option<LocationEntry> {
        self.filtered().get(self.picker.index()).cloned().cloned()
    }

    pub(crate) fn sync_len(&mut self) {
        let len = self.filtered().len();
        self.picker.set_len(len);
    }
}

impl ModelPicker {
    pub(crate) fn filtered(&self) -> Vec<&ModelInfo> {
        let needle = self.filter.to_lowercase();
        self.models
            .iter()
            .filter(|m| needle.is_empty() || m.id.to_lowercase().contains(&needle))
            .collect()
    }

    /// The id under the cursor, if any model matches the filter.
    /// The selected model's id and catalog-reported context window.
    pub(crate) fn selected_info(&self) -> Option<(String, Option<u64>)> {
        let filtered = self.filtered();
        filtered
            .get(self.index.min(filtered.len().saturating_sub(1)))
            .map(|m| (m.id.clone(), m.context_length))
    }
}

/// A modal selector rendered in the live region. Approval prompts win
/// over overlays; overlays win over the slash palette.
pub(crate) enum Overlay {
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
    /// Session mode selector over `crate::mode::Mode::ALL`.
    Mode { picker: ListPicker },
    /// Vertical spacing between transcript sections.
    TranscriptSpacing { picker: ListPicker },
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
pub(crate) const SETTINGS_ROWS: usize = 7;

/// Row actions in the /sessions picker (space arms them).
pub(crate) const SESSION_ACTIONS: &[PickerAction] = &[PickerAction {
    key: 'd',
    label: "delete",
}];

/// Row actions in the /skills picker. Enter still toggles — the common
/// case stays one key — and space reveals the rest, so deleting a skill
/// is never one stray keystroke away.
pub(crate) const SKILL_ACTIONS: &[PickerAction] = &[
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
pub(crate) struct ToolRecord {
    pub(crate) call_line: String,
    pub(crate) tool_name: String,
    pub(crate) output: serde_json::Value,
    /// Folded inner tool log of a finished subagent call, empty otherwise.
    pub(crate) inner: Vec<String>,
}

/// A completed turn's compacted work rail. The transcript keeps only the
/// summary until the user asks to inspect the tree with ctrl+o.
pub(crate) struct CompletedWork {
    pub(crate) turn: usize,
    pub(crate) summaries: Vec<String>,
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) expanded: bool,
}

pub(crate) struct ThinkingRecord {
    pub(crate) elapsed: Duration,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewMode {
    Classic,
    Split,
}

impl ViewMode {
    pub(crate) const ALL: [Self; 2] = [Self::Classic, Self::Split];

    pub(crate) fn stored() -> Self {
        match crate::config::stored_view().as_deref() {
            Some("split") => Self::Split,
            _ => Self::Classic,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::Split => "Split",
        }
    }

    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Split => "split",
        }
    }
}

#[derive(Clone)]
pub(crate) struct ToolActivity {
    /// The model-assigned tool-call id (anchors nested subagent spawns).
    pub(crate) call_id: String,
    pub(crate) call_line: String,
    pub(crate) tool_name: String,
    pub(crate) input: serde_json::Value,
    pub(crate) started: Instant,
    pub(crate) elapsed: Option<Duration>,
    pub(crate) output: Option<serde_json::Value>,
    pub(crate) is_error: bool,
    pub(crate) approval: Option<String>,
}

pub(crate) struct InspectorBodyCache {
    pub(crate) call_id: String,
    pub(crate) complete: bool,
    pub(crate) is_error: bool,
    pub(crate) width: usize,
    pub(crate) lines: Vec<Line<'static>>,
}

/// One spawned inner agent's tool activity while it runs.
pub(crate) struct SpawnActivity {
    /// Tool-call id of the subagent call that spawned it.
    pub(crate) call_id: String,
    pub(crate) parent_id: Option<u64>,
    pub(crate) depth: u32,
    pub(crate) tools: Vec<ToolActivity>,
    /// Inner call id -> index into `tools`.
    pub(crate) pending: HashMap<String, usize>,
}

pub(crate) enum RunState {
    Idle,
    Running {
        started: Instant,
        cancel: CancellationToken,
    },
}

pub(crate) enum HeldInput {
    Text(String),
    Image { label: String, image: Image },
}

impl HeldInput {
    pub(crate) fn marker(&self, index: usize) -> String {
        match self {
            Self::Text(text) => {
                let lines = text.lines().count().max(1);
                let unit = if lines == 1 { "line" } else { "lines" };
                format!("[Pasted text #{index}, {lines} {unit}]")
            }
            Self::Image { label, .. } => format!("[▧ {label}]"),
        }
    }
}

pub(crate) struct App {
    pub(crate) cfg: TuiConfig,
    /// Lines waiting to move into the transcript on the next tick.
    pub(crate) pending_history: Vec<Line<'static>>,
    /// The full session transcript (capped at [`super::TRANSCRIPT_CAP`]).
    pub(crate) transcript: Vec<Line<'static>>,
    /// Lines scrolled up from the bottom of the transcript.
    pub(crate) scroll: usize,
    /// When the reader last scrolled. Drives the transient selection hint.
    pub(crate) scroll_hint_at: Option<Instant>,
    /// Previous wrapped overflow height. Used to keep the same top row
    /// anchored while live content or the composer region changes size.
    pub(crate) transcript_max_scroll: usize,
    pub(crate) reasoning: String,
    /// When the current reasoning phase started streaming.
    pub(crate) reasoning_started: Option<Instant>,
    pub(crate) thinking_log: Vec<ThinkingRecord>,
    pub(crate) text: String,
    pub(crate) pending_assistant: Option<String>,
    pub(crate) assistant_started: bool,
    pub(crate) run: RunState,
    pub(crate) approval: Option<ApprovalRequest>,
    /// Active structured clarification form requested by the `ask` tool.
    pub(crate) ask: Option<AskForm>,
    pub(crate) composer: String,
    pub(crate) cursor: usize,
    /// Prompts waiting for the active turn to finish, oldest first.
    /// Held in marker form; expanded against [`App::pastes`] on send.
    pub(crate) prompt_queue: VecDeque<String>,
    /// Held composer entities, indexed by their stable marker number.
    /// Entries are never removed because queued and recalled prompts can
    /// still refer to them.
    pub(crate) pastes: Vec<HeldInput>,
    pub(crate) prompt_history: Vec<String>,
    pub(crate) history_pos: Option<usize>,
    pub(crate) tokens_in: u64,
    pub(crate) tokens_out: u64,
    /// FX-style live turn progress: estimated submitted prompt tokens
    /// once, plus generated output reconciled to provider usage per step.
    pub(crate) turn_tokens_in: u64,
    pub(crate) turn_tokens_out: u64,
    pub(super) turn_output_settled: u64,
    pub(super) turn_reasoning_tokens: TokenEstimator,
    pub(super) turn_text_tokens: TokenEstimator,
    pub(super) turn_tool_input_tokens: TokenEstimator,
    pub(crate) cache_read_total: u64,
    pub(crate) cache_write_total: u64,
    /// Model steps that reported usage this session.
    pub(crate) usage_steps: u64,
    /// Approximate size of the model's current context, pi-style: the
    /// last step's provider-reported total, plus bytes/4 estimates for
    /// content appended since (tool results, the next prompt), plus
    /// /compact's estimate. Distinct from the cumulative session totals.
    pub(crate) context_tokens: u64,
    /// The active model's context window, when the catalog knows it.
    pub(crate) context_window: Option<u64>,
    pub(crate) spinner_frame: usize,
    pub(crate) quit: bool,
    pub(crate) tool_log: Vec<ToolRecord>,
    /// Completed per-turn work trees, newest last.
    pub(crate) work_log: Vec<CompletedWork>,
    /// Number of user turns rendered in this session.
    pub(crate) turn_count: usize,
    /// Whether the initial welcome has been replaced by user activity.
    /// Kept separate from `turn_count`: local slash commands do not create
    /// model turns, but their transcript output must still be visible.
    pub(crate) welcome_dismissed: bool,
    /// Top-level tool calls made during the current turn.
    pub(crate) turn_tool_calls: usize,
    /// Duration/tool-call summary of the last completed turn, shown in
    /// the rail above the composer until the next run starts.
    pub(crate) last_turn_summary: Option<String>,
    /// Call lines for in-flight tool calls, keyed by call id.
    pub(crate) pending_calls: HashMap<String, usize>,
    pub(crate) activity_tools: Vec<ToolActivity>,
    pub(crate) view_mode: ViewMode,
    pub(crate) split_tool: Option<usize>,
    /// Last inspected call retained across model phases so Split never
    /// collapses or flashes while the next call is being prepared.
    pub(crate) split_snapshot: Option<ToolActivity>,
    pub(crate) split_inspector_cache: Option<InspectorBodyCache>,
    pub(crate) split_focused: bool,
    pub(crate) split_scroll: u16,
    /// Live inner activity of running subagents, keyed by spawn id.
    pub(crate) subagent_activity: HashMap<u64, SpawnActivity>,
    /// Selected row in the slash-command palette.
    pub(crate) palette_index: usize,
    /// Open modal selector, if any.
    pub(crate) overlay: Option<Overlay>,
    /// Filter to seed the model picker with once the catalog reply arrives.
    pub(crate) picker_pending: Option<String>,
    /// Text waiting to be handed to the terminal's clipboard: decided
    /// here, written by the run loop between frames.
    pub(crate) clipboard_pending: Option<String>,
    /// The last complete answer the model produced, kept verbatim so
    /// `/copy` yields markdown source rather than the wrapped, styled,
    /// syntax-highlighted lines the transcript holds.
    pub(crate) last_answer: Option<String>,
}
