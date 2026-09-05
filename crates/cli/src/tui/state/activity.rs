//! Shared tool lifecycle used by parent and spawned-agent transcripts.

use std::time::{Duration, Instant};

/// Where a tool call stands, as the transcript marks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolStatus {
    Running,
    Done,
    Failed,
    /// Never finished and the run is over: interrupted or dropped.
    Abandoned,
}

#[derive(Clone)]
pub(crate) struct ToolActivity {
    /// The model-assigned tool-call id (anchors nested subagent spawns).
    pub(crate) call_id: String,
    pub(crate) call_line: String,
    pub(crate) tool_name: String,
    pub(crate) input: serde_json::Value,
    pub(crate) started: Instant,
    pub(crate) execution_started: Option<Instant>,
    pub(crate) execution_elapsed: Option<Duration>,
    pub(crate) elapsed: Option<Duration>,
    pub(crate) output: Option<serde_json::Value>,
    pub(crate) is_error: bool,
    pub(crate) approval: Option<String>,
}

impl ToolActivity {
    pub(crate) fn new(call_id: String, tool_name: String, input: serde_json::Value) -> Self {
        Self {
            call_id,
            call_line: crate::view::tool_call_line(&tool_name, &input),
            tool_name,
            input,
            started: Instant::now(),
            execution_started: None,
            execution_elapsed: None,
            elapsed: None,
            output: None,
            is_error: false,
            approval: None,
        }
    }

    pub(crate) fn execution_started(&mut self) {
        self.execution_started.get_or_insert_with(Instant::now);
    }

    pub(crate) fn finish(&mut self, is_error: bool) {
        let now = Instant::now();
        self.elapsed = Some(now.duration_since(self.started));
        self.execution_elapsed = self
            .execution_started
            .map(|started| now.duration_since(started));
        self.is_error = is_error;
    }

    pub(crate) fn record_result(&mut self, output: serde_json::Value, is_error: bool) {
        let now = Instant::now();
        self.elapsed
            .get_or_insert_with(|| now.duration_since(self.started));
        if self.execution_elapsed.is_none() {
            self.execution_elapsed = self
                .execution_started
                .map(|started| now.duration_since(started));
        }
        self.output = Some(output);
        self.is_error = is_error;
    }

    pub(crate) fn status(&self, live: bool) -> ToolStatus {
        let finished = self.output.is_some() || self.elapsed.is_some();
        match (finished, self.is_error, live) {
            (true, true, _) => ToolStatus::Failed,
            (true, false, _) => ToolStatus::Done,
            (false, _, true) => ToolStatus::Running,
            (false, _, false) => ToolStatus::Abandoned,
        }
    }
}
