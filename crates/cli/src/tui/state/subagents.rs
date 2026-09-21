//! Spawn activity and retained agent-browser state.

use super::subagent_history::{
    bounded_text, compact_activity, history_entry_bytes, tool_history_bytes, THINKING_HISTORY,
    TRANSCRIPT_HISTORY_BYTES,
};
use super::{ThinkingRecord, ToolActivity};
use crate::tui::components::picker::ListPicker;
use ratatui::text::Line;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) struct SubagentDisplay {
    pub(crate) task: String,
    pub(crate) identity: orca_harness_tools::SubagentIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubagentTranscriptStatus {
    /// Spawned but not yet executing: a background worker waiting for a
    /// slot under the `/subagents` limit. Foreground workers pass through
    /// this state in the same tick they start.
    Queued,
    Running,
    /// Persistent sidekick is alive with its conversation retained.
    Idle,
    /// Persistent sidekick context was released; display history remains.
    Stopped,
    Completed,
    Failed,
}

impl SubagentTranscriptStatus {
    /// Still to produce a result, queued or running.
    pub(crate) fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

pub(crate) enum SubagentTranscriptEntry {
    Assistant(String),
    Activity {
        thinking: Vec<ThinkingRecord>,
        tools: Vec<ToolActivity>,
    },
    Error(String),
}

/// Bounded semantic transcript for one spawned agent. Rendering stays
/// width-independent so the browser can resize without corrupting history.
pub(crate) struct SubagentTranscript {
    pub(crate) id: u64,
    pub(crate) parent_id: Option<u64>,
    pub(crate) depth: u32,
    pub(crate) call_id: String,
    pub(crate) task: String,
    pub(crate) identity: Option<orca_harness_tools::SubagentIdentity>,
    pub(crate) status: SubagentTranscriptStatus,
    pub(crate) detached: bool,
    pub(crate) persistent: bool,
    /// The initial awaited task returned a handle, so later task failures
    /// leave this sidekick reusable. `persistent` alone only records intent.
    pub(crate) sidekick_established: bool,
    pub(crate) started: Instant,
    pub(crate) elapsed: Option<Duration>,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) reasoning_tokens: Option<u64>,
    pub(crate) entries: VecDeque<SubagentTranscriptEntry>,
    pub(crate) revision: u64,
    pub(crate) omitted_activity: usize,
    pub(crate) retained_history_bytes: usize,
    pub(crate) streaming_assistant: String,
    pub(crate) streaming_reasoning: String,
    pub(crate) reasoning_started: Option<Instant>,
    pub(crate) thinking_log: Vec<ThinkingRecord>,
    pub(crate) activity_tools: Vec<ToolActivity>,
    pub(crate) pending_calls: HashMap<String, usize>,
}

const SUBAGENT_TRANSCRIPT_CAP: usize = 200;

impl SubagentTranscript {
    pub(crate) fn new(
        id: u64,
        parent_id: Option<u64>,
        depth: u32,
        call_id: String,
        task: String,
        identity: Option<orca_harness_tools::SubagentIdentity>,
    ) -> Self {
        Self {
            id,
            parent_id,
            depth,
            call_id,
            task: bounded_text(&task),
            identity,
            status: SubagentTranscriptStatus::Queued,
            detached: false,
            persistent: false,
            sidekick_established: false,
            started: Instant::now(),
            elapsed: None,
            input_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: None,
            entries: VecDeque::new(),
            revision: 0,
            omitted_activity: 0,
            retained_history_bytes: 0,
            streaming_assistant: String::new(),
            streaming_reasoning: String::new(),
            reasoning_started: None,
            thinking_log: Vec::new(),
            activity_tools: Vec::new(),
            pending_calls: HashMap::new(),
        }
    }

    pub(crate) fn push_entry(&mut self, mut entry: SubagentTranscriptEntry) {
        // A single tool batch must also fit: keep its newest settled rows.
        let mut bytes = history_entry_bytes(&entry);
        if let SubagentTranscriptEntry::Activity { tools, .. } = &mut entry {
            let mut discard = 0;
            for tool in tools.iter() {
                if bytes <= TRANSCRIPT_HISTORY_BYTES {
                    break;
                }
                bytes = bytes.saturating_sub(tool_history_bytes(tool));
                discard += 1;
            }
            tools.drain(..discard);
            self.omitted_activity = self.omitted_activity.saturating_add(discard);
        }
        self.revision = self.revision.wrapping_add(1);
        self.retained_history_bytes = self.retained_history_bytes.saturating_add(bytes);
        self.entries.push_back(entry);
        while self.entries.len() > SUBAGENT_TRANSCRIPT_CAP
            || self.retained_history_bytes > TRANSCRIPT_HISTORY_BYTES
        {
            if let Some(entry) = self.entries.pop_front() {
                self.retained_history_bytes = self
                    .retained_history_bytes
                    .saturating_sub(history_entry_bytes(&entry));
                self.omitted_activity = self.omitted_activity.saturating_add(1);
            } else {
                break;
            }
        }
    }

    pub(crate) fn flush_reasoning(&mut self) {
        if self.streaming_reasoning.trim().is_empty() {
            self.streaming_reasoning.clear();
            self.reasoning_started = None;
            return;
        }
        self.streaming_reasoning.clear();
        let elapsed = self
            .reasoning_started
            .take()
            .map(|started| started.elapsed())
            .unwrap_or_default();
        if self.thinking_log.len() == THINKING_HISTORY {
            self.thinking_log.remove(0);
            self.omitted_activity = self.omitted_activity.saturating_add(1);
        }
        self.thinking_log.push(ThinkingRecord { elapsed });
    }

    pub(crate) fn commit_activity(&mut self) {
        self.flush_reasoning();
        if self.thinking_log.is_empty() && self.activity_tools.is_empty() {
            return;
        }
        // Assistant events can precede out-of-order results. Keep pending calls
        // live and commit only settled history so their result indices stay valid.
        let all_tools = std::mem::take(&mut self.activity_tools);
        let mut tools = Vec::new();
        for tool in all_tools {
            if let Some(index) = self.pending_calls.get_mut(&tool.call_id) {
                *index = self.activity_tools.len();
                self.activity_tools.push(tool);
            } else {
                tools.push(tool);
            }
        }
        let thinking = std::mem::take(&mut self.thinking_log);
        if !thinking.is_empty() || !tools.is_empty() {
            self.push_entry(SubagentTranscriptEntry::Activity { thinking, tools });
        }
    }

    pub(crate) fn compact_activity(&mut self) {
        self.omitted_activity = self.omitted_activity.saturating_add(compact_activity(
            &mut self.activity_tools,
            &mut self.pending_calls,
        ));
    }

    pub(crate) fn finish_activity(&mut self) {
        self.pending_calls.clear();
        self.compact_activity();
        self.commit_activity();
    }

    pub(crate) fn commit_settled_activity(&mut self) {
        if self.pending_calls.is_empty() {
            self.commit_activity();
        }
    }

    pub(crate) fn push_assistant(&mut self, message: String) {
        let message = bounded_text(&message);
        if message.trim().is_empty() {
            return;
        }
        let duplicate = self.entries.back().is_some_and(
            |entry| matches!(entry, SubagentTranscriptEntry::Assistant(text) if text == &message),
        );
        if !duplicate {
            self.push_entry(SubagentTranscriptEntry::Assistant(message));
        }
    }

    pub(crate) fn latest_answer(&self) -> Option<&str> {
        if !self.streaming_assistant.is_empty() {
            return Some(&self.streaming_assistant);
        }
        self.entries.iter().rev().find_map(|entry| match entry {
            SubagentTranscriptEntry::Assistant(text) => Some(text.as_str()),
            _ => None,
        })
    }
}

/// Which agents the browser lists; tab cycles through them in this order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum AgentTab {
    #[default]
    Running,
    Done,
    Failed,
    All,
}

impl AgentTab {
    pub(crate) const ALL: [Self; 4] = [Self::Running, Self::Done, Self::Failed, Self::All];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Done => "Done",
            Self::Failed => "Failed",
            Self::All => "All",
        }
    }

    pub(crate) fn next(self) -> Self {
        let index = Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// Running also covers queued: both are agents still to answer.
    pub(crate) fn admits(self, status: SubagentTranscriptStatus) -> bool {
        match self {
            Self::Running => status.is_active(),
            Self::Done => matches!(
                status,
                SubagentTranscriptStatus::Idle
                    | SubagentTranscriptStatus::Stopped
                    | SubagentTranscriptStatus::Completed
            ),
            Self::Failed => status == SubagentTranscriptStatus::Failed,
            Self::All => true,
        }
    }
}

/// A single selected terminal transcript; visible lines alone are cloned during redraw.
pub(crate) struct AgentBodyCache {
    pub(crate) id: u64,
    pub(crate) revision: u64,
    pub(crate) width: usize,
    pub(crate) theme: crate::view::ThemeName,
    pub(crate) style: crate::view::glyphs::UiStyle,
    pub(crate) lines: Arc<Vec<Line<'static>>>,
}

pub(crate) struct AgentBrowser {
    pub(crate) picker: ListPicker,
    pub(crate) scroll: usize,
    pub(crate) tab: AgentTab,
    pub(crate) body_cache: Option<AgentBodyCache>,
    pub(crate) table_cache: Option<crate::tui::render::AgentTableCache>,
}

impl AgentBrowser {
    pub(crate) fn new(agent_count: usize) -> Self {
        Self {
            picker: ListPicker::new(agent_count),
            scroll: 0,
            tab: AgentTab::default(),
            body_cache: None,
            table_cache: None,
        }
    }
}

/// One spawned inner agent's tool activity while it runs.
pub(crate) struct SpawnActivity {
    /// Tool-call id of the subagent call that spawned it.
    pub(crate) call_id: String,
    pub(crate) parent_id: Option<u64>,
    pub(crate) depth: u32,
    pub(crate) task: String,
    pub(crate) identity: Option<orca_harness_tools::SubagentIdentity>,
    pub(crate) tools: Vec<ToolActivity>,
    pub(crate) omitted_tools: usize,
    /// Inner call id -> index into `tools`.
    pub(crate) pending: HashMap<String, usize>,
}
