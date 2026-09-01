//! The `App` state transitions: construction, transcript/activity
//! bookkeeping, and the small helpers the terminal loop and command
//! handlers call. Pure data manipulation — nothing here touches the
//! terminal or the worker.

use std::collections::VecDeque;

use ratatui::text::Line;

use crate::tui::components::message::{assistant_message, user_prompt};
use crate::tui::components::picker::ListPicker;
use crate::tui::components::transcript::{
    append_block, line_is_blank, set_transcript_spacing, BlockSpacing, TranscriptSpacing,
};
use crate::view::theme;

use super::render::{activity_lines_selected, collapsed_activity_lines};
use super::state::{App, CompletedWork, RunState, ThinkingRecord, ToolRecord, TuiConfig, ViewMode};
use super::{elapsed_label, line_text, SCROLL_HINT, TRANSCRIPT_CAP};

impl App {
    pub(crate) fn new(cfg: TuiConfig) -> Self {
        let spacing = crate::config::stored_transcript_spacing()
            .as_deref()
            .and_then(TranscriptSpacing::from_slug)
            .unwrap_or(TranscriptSpacing::Comfortable);
        // The preference is process-global; serialize the write against
        // tests that transition the picker live (see `SPACING_GUARD`).
        let _guard = super::SPACING_GUARD
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        set_transcript_spacing(spacing);
        Self {
            cfg,
            reasoning_effort: None,
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
            ask: None,
            composer: String::new(),
            cursor: 0,
            prompt_queue: VecDeque::new(),
            pastes: Vec::new(),
            prompt_history: Vec::new(),
            history_pos: None,
            tokens_in: 0,
            tokens_out: 0,
            turn_tokens_in: 0,
            turn_tokens_out: 0,
            turn_output_settled: 0,
            turn_reasoning_tokens: Default::default(),
            turn_text_tokens: Default::default(),
            turn_tool_input_tokens: Default::default(),
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
            welcome_dismissed: false,
            turn_tool_calls: 0,
            last_turn_summary: None,
            pending_calls: std::collections::HashMap::new(),
            activity_tools: Vec::new(),
            view_mode: ViewMode::stored(),
            split_tool: None,
            split_snapshot: None,
            split_inspector_cache: None,
            inspector_mode: super::state::InspectorMode::stored(),
            split_scroll: 0,
            inspector_area: None,
            subagent_activity: std::collections::HashMap::new(),
            subagent_display: std::collections::HashMap::new(),
            palette_picker: ListPicker::new(super::command_catalog::COMMANDS.len()),
            overlay: None,
            overlay_stack: Vec::new(),
            picker_pending: None,
            next_picker_request: 1,
            clipboard_pending: None,
            last_answer: None,
        }
    }

    pub(crate) fn running(&self) -> bool {
        matches!(self.run, RunState::Running { .. })
    }

    /// The palette is open whenever the composer starts with `/` and no
    /// approval or clarification prompt is pending. Returns the text after the slash.
    pub(crate) fn palette_query(&self) -> Option<&str> {
        if self.approval.is_some() || self.ask.is_some() {
            return None;
        }
        self.composer.strip_prefix('/')
    }

    pub(crate) fn reset_palette_picker(&mut self) {
        let len = self
            .palette_query()
            .map(super::command_catalog::filter_commands)
            .map_or(0, |commands| commands.len());
        self.palette_picker = ListPicker::new(len);
    }

    pub(crate) fn push_line(&mut self, line: Line<'static>) {
        self.pending_history.push(line);
    }

    pub(crate) fn push_user_prompt(&mut self, text: &str, width: usize) {
        self.pending_history
            .extend(user_prompt(text, width, theme().strong));
    }

    pub(crate) fn push_record(&mut self, record: ToolRecord) {
        self.tool_log.push(record);
        if self.tool_log.len() > 100 {
            self.tool_log.remove(0);
        }
    }

    /// Finish one reasoning phase. The activity rail owns its compact
    /// rendering; the full text remains available through expansion.
    pub(crate) fn flush_reasoning(&mut self) {
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

    fn refresh_turn_output(&mut self) {
        self.turn_tokens_out = self
            .turn_output_settled
            .saturating_add(self.turn_reasoning_tokens.estimate())
            .saturating_add(self.turn_text_tokens.estimate())
            .saturating_add(self.turn_tool_input_tokens.estimate());
    }

    pub(crate) fn consume_turn_reasoning(&mut self, text: &str) {
        self.turn_reasoning_tokens.consume(text);
        self.refresh_turn_output();
    }

    pub(crate) fn consume_turn_text(&mut self, text: &str) {
        self.turn_text_tokens.consume(text);
        self.refresh_turn_output();
    }

    pub(crate) fn consume_turn_tool_input(&mut self, text: &str) {
        self.turn_tool_input_tokens.consume(text);
        self.refresh_turn_output();
    }

    pub(crate) fn consume_turn_text_if_unseen(&mut self, text: &str) {
        if self.turn_text_tokens.estimate() == 0 {
            self.consume_turn_text(text);
        }
    }

    /// Replace the active model step's estimates with authoritative
    /// provider output, then start fresh estimators for the next step.
    pub(crate) fn reconcile_turn_output(&mut self, output_tokens: u64) {
        self.turn_output_settled = self.turn_output_settled.saturating_add(output_tokens);
        self.turn_reasoning_tokens = Default::default();
        self.turn_text_tokens = Default::default();
        self.turn_tool_input_tokens = Default::default();
        self.refresh_turn_output();
    }

    /// Preserve estimates when a provider omitted usage for this step.
    pub(crate) fn settle_turn_output_estimate(&mut self) {
        let estimate = self
            .turn_reasoning_tokens
            .estimate()
            .saturating_add(self.turn_text_tokens.estimate())
            .saturating_add(self.turn_tool_input_tokens.estimate());
        self.turn_output_settled = self.turn_output_settled.saturating_add(estimate);
        self.turn_reasoning_tokens = Default::default();
        self.turn_text_tokens = Default::default();
        self.turn_tool_input_tokens = Default::default();
        self.refresh_turn_output();
    }

    pub(crate) fn reset_activity(&mut self) {
        self.turn_tool_calls = 0;
        self.turn_tokens_in = 0;
        self.turn_tokens_out = 0;
        self.turn_output_settled = 0;
        self.turn_reasoning_tokens = Default::default();
        self.turn_text_tokens = Default::default();
        self.turn_tool_input_tokens = Default::default();
        self.last_turn_summary = None;
        self.reasoning.clear();
        self.reasoning_started = None;
        self.thinking_log.clear();
        self.activity_tools.clear();
        self.split_tool = None;
        self.split_scroll = 0;
        self.subagent_activity.clear();
        self.subagent_display.clear();
        self.pending_calls.clear();
        self.pending_assistant = None;
        self.assistant_started = false;
    }

    /// Render one transcript component with normalized outer edges and a
    /// single source of truth for vertical rhythm.
    pub(crate) fn push_transcript_block(
        &mut self,
        lines: Vec<Line<'static>>,
        spacing: BlockSpacing,
    ) {
        let spacing = if self.assistant_started {
            spacing
        } else {
            BlockSpacing::Section
        };
        let transcript_tail = self.transcript.last().map(line_is_blank);
        let appended = append_block(&mut self.pending_history, lines, spacing, transcript_tail);
        self.assistant_started |= appended;
    }

    pub(crate) fn push_markdown_block(&mut self, text: &str, width: usize, spacing: BlockSpacing) {
        self.push_transcript_block(assistant_message(text, width), spacing);
    }

    pub(crate) fn commit_activity(&mut self, width: usize) {
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
            // Each work phase is its own compact rail. Give it one blank row
            // from the preceding prose or rail, while keeping the rows inside
            // the rail itself tight.
            self.push_transcript_block(lines.clone(), BlockSpacing::Section);
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
        self.subagent_display.clear();
        self.pending_calls.clear();
        self.split_tool = None;
        self.split_scroll = 0;
    }

    /// A new model phase can only begin after the previous batch of tools
    /// has settled. Commit that batch before accepting new deltas so the
    /// transcript retains the event stream's chronology.
    pub(crate) fn commit_settled_tools(&mut self, width: usize) {
        if !self.activity_tools.is_empty() && self.pending_calls.is_empty() {
            self.commit_activity(width);
        }
    }

    /// Whether the post-scroll selection hint is still within its window.
    pub(crate) fn scroll_hint_live(&self) -> bool {
        self.scroll_hint_at
            .is_some_and(|at| at.elapsed() < SCROLL_HINT)
    }

    /// Move pending lines into the transcript. A reader who has scrolled
    /// up stays anchored; the bottom follows new content otherwise.
    pub(crate) fn absorb_pending(&mut self) {
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
