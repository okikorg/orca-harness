//! The interactive terminal's persistent state: configuration handed in
//! from the runner, the full-screen [`App`] structure, and the modal
//! selectors (model/location/provider/theme/view/spacing/usage/api-key/
//! settings/approvals/extensions/mcp/plugins/skills/sessions) that overlays can
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
use orca_harness_model_providers::SupportedEfforts;

use super::format::TokenEstimator;

mod activity;
pub(crate) mod subagent_history;
mod subagents;
pub(crate) use activity::{ToolActivity, ToolStatus};
pub(crate) use subagents::{
    AgentBodyCache, AgentBrowser, AgentTab, SpawnActivity, SubagentDisplay, SubagentTranscript,
    SubagentTranscriptEntry, SubagentTranscriptStatus,
};

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
/// filter, and the standard shared picker state for the filtered view.
pub(crate) struct ModelPicker {
    pub(crate) subagent: Option<(String, crate::Provider)>,
    pub(crate) models: Vec<ModelInfo>,
    pub(crate) filter: String,
    pub(crate) picker: ListPicker,
}

/// The picker to open when a model-catalog request completes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ModelPickerTarget {
    Models { filter: String },
    ActiveModelEffort,
    Subagent {
        tier: String,
        provider: crate::Provider,
    },
}

/// Provider-advertised reasoning efforts for the active or newly chosen model.
pub(crate) struct EffortPicker {
    pub(crate) model_id: String,
    pub(crate) context_window: Option<u64>,
    pub(crate) efforts: Vec<String>,
    pub(crate) picker: ListPicker,
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

/// An enabled, loaded skill inserted into the composer by the `$` picker.
pub(crate) struct SkillMentionPicker {
    pub(crate) entries: Vec<crate::skills::SkillEntry>,
    pub(crate) query: String,
    /// Character offset of the `$` that opened this picker.
    pub(crate) token_start: usize,
    pub(crate) picker: ListPicker,
}

impl LocationPicker {
    pub(crate) fn filtered(&self) -> Vec<&LocationEntry> {
        let needle = self.query.to_lowercase();
        self.entries
            .iter()
            .filter(|entry| needle.is_empty() || entry.path.to_lowercase().contains(&needle))
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

impl SkillMentionPicker {
    pub(crate) fn filtered(&self) -> Vec<&crate::skills::SkillEntry> {
        let needle = self.query.to_lowercase();
        self.entries
            .iter()
            .filter(|entry| needle.is_empty() || entry.name.to_lowercase().contains(&needle))
            .collect()
    }

    pub(crate) fn selected(&self) -> Option<String> {
        self.filtered()
            .get(self.picker.index())
            .map(|entry| entry.name.clone())
    }

    pub(crate) fn sync_len(&mut self) {
        let len = self.filtered().len();
        self.picker.set_len(len);
    }
}

impl ModelPicker {
    pub(crate) fn new(models: Vec<ModelInfo>, filter: String) -> Self {
        let needle = filter.to_lowercase();
        let len = models
            .iter()
            .filter(|model| needle.is_empty() || model.id.to_lowercase().contains(&needle))
            .count();
        Self {
            subagent: None,
            models,
            filter,
            picker: ListPicker::new(len),
        }
    }

    pub(crate) fn filtered(&self) -> Vec<&ModelInfo> {
        let needle = self.filter.to_lowercase();
        self.models
            .iter()
            .filter(|m| needle.is_empty() || m.id.to_lowercase().contains(&needle))
            .collect()
    }

    /// The provider metadata under the cursor, if any model matches.
    pub(crate) fn selected_model(&self) -> Option<ModelInfo> {
        let filtered = self.filtered();
        filtered.get(self.picker.index()).cloned().cloned()
    }

    pub(crate) fn reset_filtered_selection(&mut self) {
        self.picker = ListPicker::new(self.filtered().len());
    }
}

impl EffortPicker {
    pub(crate) fn new(model: ModelInfo) -> Option<Self> {
        let reasoning = model.reasoning?;
        let SupportedEfforts::Listed(efforts) = reasoning.supported_efforts? else {
            return None;
        };
        if efforts.is_empty() {
            return None;
        }
        let selected = reasoning
            .default_effort
            .as_ref()
            .and_then(|default| efforts.iter().position(|effort| effort == default))
            .unwrap_or(0);
        Some(Self {
            model_id: model.id,
            context_window: model.context_length,
            picker: ListPicker::with_selected(efforts.len(), selected),
            efforts,
        })
    }

    pub(crate) fn selected(&self) -> Option<String> {
        self.efforts.get(self.picker.index()).cloned()
    }
}

/// A modal selector rendered in the live region. Approval prompts win
/// over overlays; overlays win over the slash palette.
pub(crate) enum Overlay {
    /// Browsable slash-command reference; enter places a command in the composer.
    Help { filter: String, picker: ListPicker },
    /// Model selector over the fetched catalog.
    Models(ModelPicker),
    /// Reasoning-effort selector for a model that advertises choices.
    Efforts(EffortPicker),
    /// Workspace file/folder selector opened by typing `@` in the composer.
    Locations(LocationPicker),
    /// Enabled skill selector opened by typing `$` in the composer.
    SkillMentions(SkillMentionPicker),
    /// Provider selector (OpenRouter, Vercel AI Gateway, CheaperInference,
    /// OpenAI, Codex, local).
    Providers { picker: ListPicker },
    /// Theme selector over `view::ThemeName::ALL`.
    Themes { picker: ListPicker },
    /// Transcript layout selector (classic or split inspector).
    Views { picker: ListPicker },
    /// Session mode selector over `crate::mode::Mode::ALL`.
    Mode { picker: ListPicker },
    /// Vertical spacing between transcript sections.
    TranscriptSpacing { picker: ListPicker },
    /// The mark vocabulary: minimal or glyph.
    Style { picker: ListPicker },
    /// Default split Tool Inspector rendering.
    Inspector { picker: ListPicker },
    /// Read-only session usage panel; any dismissal key closes it.
    Usage,
    /// Masked API-key entry for a provider whose key is not in the env.
    ApiKey { provider: Provider, input: String },
    /// Settings menu: shows the persisted preferences and jumps into
    /// the provider, model, theme, api-key, and approval pickers.
    Settings { picker: ListPicker },
    /// Session-live subagent governance menu.
    Subagents { picker: ListPicker },
    /// Route or model choices for one subagent setting row.
    SubagentValues {
        setting: SubagentSetting,
        values: Vec<String>,
        picker: ListPicker,
    },
    SubagentNumber {
        setting: SubagentSetting,
        input: String,
        error: String,
    },
    /// This workspace's saved always-allowed tools; enter revokes one.
    Approvals {
        tools: Vec<String>,
        picker: ListPicker,
    },
    /// The harness extension catalog; enter toggles the selected one.
    Extensions { picker: ListPicker },
    /// Standalone and plugin MCP servers. Standalone rows toggle on or
    /// off; plugin rows are read-only and direct management to `/plugin`.
    Mcp {
        entries: Vec<crate::tui::mcp_picker::Entry>,
        filter: String,
        picker: ListPicker,
    },
    /// Registered Agent Plugins with saved and process-live state separated.
    Plugins {
        entries: Vec<crate::config::RegisteredPlugin>,
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

pub(crate) use crate::subagent_settings::{SubagentSetting, SUBAGENT_ROWS};

/// Rows in the settings overlay: provider, model, theme, transcript view,
/// api key, approvals.
pub(crate) const SETTINGS_ROWS: usize = 9;

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

#[derive(Clone)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InspectorMode {
    Summary,
    Debug,
}

impl InspectorMode {
    pub(crate) const ALL: [Self; 2] = [Self::Summary, Self::Debug];

    pub(crate) fn stored() -> Self {
        match crate::config::stored_inspector().as_deref() {
            Some("debug") => Self::Debug,
            _ => Self::Summary,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Summary => "Summary",
            Self::Debug => "Debug",
        }
    }

    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Debug => "debug",
        }
    }
}

pub(crate) struct InspectorBodyCache {
    pub(crate) call_id: String,
    pub(crate) complete: bool,
    pub(crate) has_output: bool,
    pub(crate) is_error: bool,
    pub(crate) width: usize,
    pub(crate) mode: InspectorMode,
    pub(crate) lines: Vec<Line<'static>>,
}

pub(crate) enum RunState {
    Idle,
    Running {
        id: crate::msg::RunId,
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
                format!("[▤ Pasted text #{index}, {lines} {unit}]")
            }
            Self::Image { label, .. } => format!("[▧ {label}]"),
        }
    }
}

pub(crate) struct App {
    pub(crate) cfg: TuiConfig,
    /// The workspace's branch captured when the TUI starts. `None` covers
    /// non-Git directories and detached HEADs without adding Git work to
    /// every frame.
    pub(crate) git_branch: Option<String>,
    /// Explicit reasoning effort chosen for the active model. `None` lets
    /// the provider use its advertised default.
    pub(crate) reasoning_effort: Option<String>,
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
    pub(crate) next_run_id: u64,
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
    /// Call lines for in-flight tool calls, keyed by call id.
    pub(crate) pending_calls: HashMap<String, usize>,
    pub(crate) activity_tools: Vec<ToolActivity>,
    pub(crate) view_mode: ViewMode,
    pub(crate) split_tool: Option<usize>,
    /// Last inspected call retained across model phases so Split never
    /// collapses or flashes while the next call is being prepared.
    pub(crate) split_snapshot: Option<ToolActivity>,
    pub(crate) split_inspector_cache: Option<InspectorBodyCache>,
    pub(crate) inspector_mode: InspectorMode,
    pub(crate) split_scroll: u16,
    /// Where the last frame drew the inspector, so the wheel can be routed
    /// to it without the event loop re-deriving the split geometry.
    pub(crate) inspector_area: Option<ratatui::layout::Rect>,
    /// Live inner activity of running subagents, keyed by spawn id.
    pub(crate) subagent_activity: HashMap<u64, SpawnActivity>,
    /// Session-visible agent history used by the live transcript browser.
    pub(crate) subagent_transcripts: HashMap<u64, SubagentTranscript>,
    pub(crate) evicted_agent_histories: usize,
    pub(crate) agent_list_cache:
        std::cell::RefCell<Option<std::sync::Arc<super::render::AgentListCache>>>,
    /// The dedicated read-only agent browser, when open.
    pub(crate) agent_browser: Option<AgentBrowser>,
    /// Down from an empty newest composer focuses the actionable agent count.
    pub(crate) agents_status_focused: bool,
    /// Resolved top-level worker identity retained until the work phase commits,
    /// including when a failed tool result has no structured identity payload.
    pub(crate) subagent_display: HashMap<String, SubagentDisplay>,
    /// Shared selection state for the slash-command palette.
    pub(crate) palette_picker: ListPicker,
    /// Open modal selector, if any.
    pub(crate) overlay: Option<Overlay>,
    /// Parent picker pages. Right/enter descends, left returns one page,
    /// and escape clears the whole flow.
    pub(crate) overlay_stack: Vec<Overlay>,
    /// Picker to open once the requested model catalog arrives.
    pub(crate) picker_pending: Option<(u64, ModelPickerTarget)>,
    pub(crate) next_picker_request: u64,
    /// Text waiting to be handed to the terminal's clipboard: decided
    /// here, written by the run loop between frames.
    pub(crate) clipboard_pending: Option<String>,
    /// The last complete answer the model produced, kept verbatim so
    /// `/copy` yields markdown source rather than the wrapped, styled,
    /// syntax-highlighted lines the transcript holds.
    pub(crate) last_answer: Option<String>,
}
