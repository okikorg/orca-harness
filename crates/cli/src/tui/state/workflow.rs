//! The submitted graph of a workflow run, held from submission rather than
//! discovered as stages spawn.
//!
//! A planned stage is not an agent: it has no transcript, no tokens and no
//! history budget, and a graph of them costs a few hundred bytes. Rows join to
//! the agents that execute them through `spawn`, so the browser can show the
//! whole graph the moment it is submitted and still open a stage's transcript
//! once one exists.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A planned stage's state. `Blocked` and `Stopped` have no counterpart in
/// [`SubagentTranscriptStatus`](super::SubagentTranscriptStatus): a stage waits
/// on the graph before any agent exists, and ends unrun when the run does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StageState {
    /// Upstream stages have not all finished.
    Blocked,
    /// Ready and spawned, waiting for a concurrency slot.
    Queued,
    Running,
    Done,
    Failed,
    /// Abandoned because the run ended: a sibling failed, or it was cancelled.
    Stopped,
}

impl StageState {
    pub(crate) fn is_active(self) -> bool {
        matches!(self, Self::Blocked | Self::Queued | Self::Running)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }

    /// The engine's own `StageStatus`, as serialized in a run outcome.
    fn from_outcome(status: &str) -> Option<Self> {
        match status {
            "pending" => Some(Self::Blocked),
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            "cancelled" | "stopped" => Some(Self::Stopped),
            _ => None,
        }
    }
}

pub(crate) struct StageRow {
    pub(crate) id: String,
    pub(crate) needs: Vec<String>,
    pub(crate) model: Option<String>,
    pub(crate) is_map: bool,
    /// The submitted prompt, before template rendering.
    pub(crate) prompt: String,
    pub(crate) state: StageState,
    /// The agent executing this stage, once one has been spawned.
    pub(crate) spawn: Option<u64>,
    pub(crate) started: Option<Instant>,
    pub(crate) elapsed: Option<Duration>,
    /// What the row says instead of the prompt: a failure, or why it waits.
    pub(crate) detail: Option<String>,
}

impl StageRow {
    pub(crate) fn elapsed(&self) -> Option<Duration> {
        self.elapsed
            .or_else(|| self.started.map(|started| started.elapsed()))
    }
}

pub(crate) struct WorkflowRun {
    pub(crate) started: Instant,
    pub(crate) elapsed: Option<Duration>,
    pub(crate) stages: Vec<StageRow>,
    index: HashMap<String, usize>,
    /// Set once the run itself reports, so the panel can stop implying work.
    pub(crate) error: Option<String>,
}

impl WorkflowRun {
    /// Build the plan from the `graph` the model submitted. Returns `None`
    /// when the argument is not a usable graph, so a malformed call simply
    /// leaves the run without a panel rather than half of one.
    pub(crate) fn from_graph(graph: &serde_json::Value) -> Option<Self> {
        let stages: Vec<StageRow> = graph
            .as_array()?
            .iter()
            .filter_map(|stage| {
                let id = stage.get("id")?.as_str()?.to_string();
                let mut needs: Vec<String> = stage
                    .get("needs")
                    .and_then(serde_json::Value::as_array)
                    .map(|needs| {
                        needs
                            .iter()
                            .filter_map(|need| need.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                // `over` is an implicit dependency the engine adds; the panel
                // must show the same edges the run will actually wait on.
                if let Some(over) = stage.get("over").and_then(serde_json::Value::as_str) {
                    if !needs.iter().any(|need| need == over) {
                        needs.push(over.to_string());
                    }
                }
                Some(StageRow {
                    id,
                    needs,
                    model: stage
                        .get("model")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    is_map: stage.get("kind").and_then(serde_json::Value::as_str) == Some("map"),
                    prompt: super::subagent_history::bounded_text(
                        stage
                            .get("prompt")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default(),
                    ),
                    state: StageState::Blocked,
                    spawn: None,
                    started: None,
                    elapsed: None,
                    detail: None,
                })
            })
            .collect();
        if stages.is_empty() {
            return None;
        }
        let index = stages
            .iter()
            .enumerate()
            .map(|(position, stage)| (stage.id.clone(), position))
            .collect();
        let mut run = Self {
            started: Instant::now(),
            elapsed: None,
            stages,
            index,
            error: None,
        };
        run.refresh_blocked();
        Some(run)
    }

    pub(crate) fn position(&self, stage: &str) -> Option<usize> {
        self.index.get(stage).copied()
    }

    /// Blocked-ness is derived here, not reported: the engine spawns a stage
    /// when its dependencies clear, so only the graph knows why one waits.
    /// Re-derived on every transition, or a satisfied stage keeps its reason.
    pub(crate) fn refresh_blocked(&mut self) {
        let done: std::collections::HashSet<&str> = self
            .stages
            .iter()
            .filter(|stage| stage.state == StageState::Done)
            .map(|stage| stage.id.as_str())
            .collect();
        let waiting: Vec<Option<String>> = self
            .stages
            .iter()
            .map(|stage| {
                if stage.state != StageState::Blocked {
                    return None;
                }
                let mut pending = stage
                    .needs
                    .iter()
                    .filter(|need| !done.contains(need.as_str()));
                pending.next().map(|first| {
                    let rest = pending.count();
                    if rest == 0 {
                        format!("blocked on {first}")
                    } else {
                        format!("blocked on {first} +{rest}")
                    }
                })
            })
            .collect();
        for (stage, reason) in self.stages.iter_mut().zip(waiting) {
            if stage.state == StageState::Blocked {
                stage.detail = reason.or_else(|| Some("queued".into()));
            }
        }
    }

    pub(crate) fn spawned(&mut self, stage: &str, spawn: u64) {
        let Some(row) = self.position(stage).map(|at| &mut self.stages[at]) else {
            return;
        };
        row.spawn = Some(spawn);
        row.state = StageState::Queued;
        row.started = Some(Instant::now());
        row.detail = None;
        self.refresh_blocked();
    }

    pub(crate) fn advance(&mut self, at: usize, state: StageState, detail: Option<String>) {
        let row = &mut self.stages[at];
        if row.state == state && detail.is_none() {
            return;
        }
        if state == StageState::Running && row.state == StageState::Queued {
            // Queue time is not stage time, exactly as a subagent row restarts
            // its clock when it takes a slot.
            row.started = Some(Instant::now());
        }
        if !state.is_active() && row.elapsed.is_none() {
            row.elapsed = row.started.map(|started| started.elapsed());
        }
        row.state = state;
        if let Some(detail) = detail {
            row.detail = Some(detail);
        }
        self.refresh_blocked();
    }

    /// Apply the run's own terminal report. The outcome's stage map is
    /// authoritative: per-stage inference cannot see a cancellation that
    /// stopped stages before they ever reported.
    pub(crate) fn settle(&mut self, message: &str, is_error: bool) {
        self.elapsed = Some(self.started.elapsed());
        let outcome = message
            .split_once('\n')
            .map_or(message, |(_, rest)| rest)
            .trim();
        let outcome: Option<serde_json::Value> = serde_json::from_str(outcome).ok();
        let reported = outcome
            .as_ref()
            .and_then(|outcome| outcome.get("workflow").or(Some(outcome)));
        if let Some(error) = reported
            .and_then(|outcome| outcome.get("error"))
            .and_then(serde_json::Value::as_str)
        {
            self.error = Some(super::subagent_history::bounded_text(error));
        } else if is_error {
            self.error = Some(super::subagent_history::bounded_text(
                message.lines().next().unwrap_or(message),
            ));
        }
        let stages = reported.and_then(|outcome| outcome.get("stages"));
        for at in 0..self.stages.len() {
            let reported = stages
                .and_then(|stages| stages.get(&self.stages[at].id))
                .and_then(serde_json::Value::as_str)
                .and_then(StageState::from_outcome);
            match reported {
                Some(state) => self.advance(at, state, None),
                None if self.stages[at].state.is_active() => {
                    self.advance(at, StageState::Stopped, Some("run ended".into()));
                }
                None => {}
            }
        }
    }

    /// Rows by state, in [`StageState`] order, for the panel's header.
    pub(crate) fn counts(&self) -> Counts {
        let mut counts = Counts::default();
        for stage in &self.stages {
            match stage.state {
                StageState::Blocked | StageState::Queued => counts.waiting += 1,
                StageState::Running => counts.running += 1,
                StageState::Done => counts.done += 1,
                StageState::Failed => counts.failed += 1,
                StageState::Stopped => counts.stopped += 1,
            }
        }
        counts
    }

    pub(crate) fn is_active(&self) -> bool {
        self.elapsed.is_none() && self.stages.iter().any(|stage| stage.state.is_active())
    }
}

#[derive(Default, Clone, Copy)]
pub(crate) struct Counts {
    pub(crate) waiting: usize,
    pub(crate) running: usize,
    pub(crate) done: usize,
    pub(crate) failed: usize,
    pub(crate) stopped: usize,
}

impl Counts {
    pub(crate) fn total(&self) -> usize {
        self.waiting + self.running + self.done + self.failed + self.stopped
    }
}
