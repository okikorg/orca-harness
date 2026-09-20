//! Session mode: the stance a run takes toward acting on the machine.
//!
//! `Normal` is the agent as usual — every registered tool is available,
//! and the gated ones ask for approval. `Plan` is read-only: the agent
//! may investigate but may not change anything, so it answers with a
//! plan instead of a diff. `Orchestrate` is delegation-first: the parent
//! investigates and delegates substantial work, but may make basic edits.
//! `Auto` clears guarded native workspace mutations and reviews actions with remaining scope or safety
//! risk against the root request. `Yolo` removes review entirely. Both
//! remain visible in the status line because neither opens ordinary
//! approval prompts.
//!
//! Plan mode is an **allowlist**, not a denylist. A denylist would have
//! to enumerate every mutating tool, and the session's tool set is not
//! knowable here — MCP servers and skills add tools this file has never
//! heard of. Naming the handful of tools that only observe fails closed:
//! an unknown tool is denied, and nothing new becomes silently allowed
//! by being added later.
//!
//! Unlike the extension, MCP, and skill toggles, flipping the mode does
//! **not** rebuild the agent: [`PlanGate`] reads the shared handle on
//! every call, so `/mode` applies to the tool call happening right now,
//! not to the next run. A mode that took effect one run late would be a
//! safety feature that lies. Do not "fix" this into a rebuild.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use orca_harness_core::{Extension, ExtensionError, Subscriptions, ToolCall, ToolDecision};

use crate::plan::PlanArea;

mod model;
mod plan_paths;
pub(crate) use model::OrchestrateModel;

#[cfg(test)]
mod model_tests;
#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    /// Read-only: only the tools in [`READ_ONLY_TOOLS`] run.
    Plan,
    /// Delegation-first: read-only tools, delegation, and native file edits
    /// are allowed. Significant implementation and testing belong to workers.
    Orchestrate,
    /// Unresolved actions are reviewed automatically against the current
    /// root request instead of opening a human approval prompt.
    Auto,
    /// Every tool runs without approval prompts. The status line shows
    /// `yolo` for the whole session.
    Yolo,
}

impl Mode {
    pub const ALL: [Mode; 5] = [
        Mode::Normal,
        Mode::Plan,
        Mode::Orchestrate,
        Mode::Auto,
        Mode::Yolo,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Plan => "plan",
            Mode::Orchestrate => "orchestrate",
            Mode::Auto => "auto",
            Mode::Yolo => "yolo",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Mode::Normal => "every tool is available; gated tools ask for approval",
            Mode::Plan => "read-only: the agent investigates and proposes, but changes nothing",
            Mode::Orchestrate => {
                "delegation-first; basic edits permitted, substantial work delegated"
            }
            Mode::Auto => {
                "safe tools and guarded file edits run; risk-bearing actions are reviewed"
            }
            Mode::Yolo => "every tool runs without approval prompts",
        }
    }

    /// Parse a label as produced by [`Mode::label`], accepting the
    /// spellings a user is likely to type.
    pub fn from_label(label: &str) -> Option<Mode> {
        match label.trim().to_ascii_lowercase().as_str() {
            "normal" | "default" | "off" | "act" => Some(Mode::Normal),
            "plan" | "planning" | "read-only" | "readonly" | "ro" => Some(Mode::Plan),
            "orchestrate" | "orchestrator" | "delegate" => Some(Mode::Orchestrate),
            "auto" | "automatic" => Some(Mode::Auto),
            "yolo" => Some(Mode::Yolo),
            _ => None,
        }
    }

    /// True when the ordinary human prompt must not open. Plan mode denies
    /// before approval; auto delegates unresolved calls to its reviewer;
    /// yolo admits them without review.
    pub fn bypasses_human_approval(self) -> bool {
        matches!(self, Mode::Auto | Mode::Yolo)
    }

    /// The stance a spawned worker inherits from the session mode.
    /// An orchestrate restriction exists to push work down, not to
    /// throttle the workers it delegates to, so a worker reads the
    /// session as normal. Plan keeps gating children — a restriction
    /// that only held at depth 0 would not be a restriction — and yolo
    /// propagates because the user silenced review for the whole tree.
    pub fn child_mode(self) -> Mode {
        match self {
            Mode::Orchestrate => Mode::Normal,
            other => other,
        }
    }
}

/// The tools that hand work to spawned workers. In orchestrate mode they
/// join [`READ_ONLY_TOOLS`] at the top level. Kept separate from the
/// read-only list on purpose: [`AutoApproval::known_safe`] reuses that
/// list, and spawning is not read-only — in auto mode a `subagent` call
/// still carries the cost and fan-out of a child run, so it stays
/// reviewed.
pub const DELEGATION_TOOLS: &[&str] = &["subagent", "workflow"];

/// The tools that only observe: they read files, search, or fetch, and
/// leave the machine exactly as they found it. Everything else — `shell`,
/// `process`, `pykernel`, `bun_repl`, `write_file`, `edit_file`, `apply_patch`,
/// `multi_edit`, `subagent`, the `fs_admin` bundle, every MCP tool — is denied
/// in plan mode.
///
/// `shell` is absent on purpose. Most of what an agent wants it for in
/// plan mode (`git log`, `cargo check`) is read-only, but deciding that
/// from a command string is guesswork, and a safety mode that guesses is
/// not a safety mode. `read_file`, `grep`, `glob`, and `list_dir` cover
/// investigation without it.
pub const READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "list_dir",
    "grep",
    "glob",
    "file_info",
    "read_tool_result",
    "memory_search",
    "web_fetch",
    "web_search",
    "web_crawl",
    "skill",
    "todo_write",
    "ask",
];

/// Cloneable handle onto the session's mode, captured by [`PlanGate`],
/// by the approval extension, and by the TUI's status line. Cheap
/// enough to read on every tool call.
#[derive(Clone, Default)]
pub struct ModeHandle(Arc<AtomicU8>);

impl ModeHandle {
    pub fn new(mode: Mode) -> Self {
        let handle = Self::default();
        handle.set(mode);
        handle
    }

    pub fn get(&self) -> Mode {
        match self.0.load(Ordering::Relaxed) {
            0 => Mode::Normal,
            1 => Mode::Plan,
            2 => Mode::Orchestrate,
            3 => Mode::Auto,
            _ => Mode::Yolo,
        }
    }

    pub fn set(&self, mode: Mode) {
        let encoded = match mode {
            Mode::Normal => 0,
            Mode::Plan => 1,
            Mode::Orchestrate => 2,
            Mode::Auto => 3,
            Mode::Yolo => 4,
        };
        self.0.store(encoded, Ordering::Relaxed);
    }

    pub fn is_plan(&self) -> bool {
        self.get() == Mode::Plan
    }
}

/// Denies every tool that is not read-only while the session is in plan
/// mode. Register it **before** the approval extension: the kernel stops
/// at the first `Deny`, so a call plan mode refuses never reaches the
/// user as an approval prompt.
///
/// The one exception is the plan area. A plan mode that cannot write its
/// plan down leaves the plan in prose the next turn has to re-derive, so
/// `write_file`, `edit_file`, `multi_edit`, and non-deleting `apply_patch`
/// calls are allowed against markdown files in `docs/plan/` — and nowhere
/// else. Which file, and whether to write one at all, is the agent's decision;
/// the gate only holds the fence.
/// Because it runs before approval, an "always allow write_file" grant
/// cannot widen past that directory.
pub struct PlanGate {
    mode: ModeHandle,
    plan: PlanArea,
    /// Workers read the session mode through [`Mode::child_mode`], so an
    /// orchestrate restriction does not strangle the workers it spawned.
    worker: bool,
}

impl PlanGate {
    pub fn new(mode: ModeHandle, plan: PlanArea) -> Self {
        Self {
            mode,
            plan,
            worker: false,
        }
    }

    /// The gate a spawned worker carries. Orchestrate's restriction exists
    /// to push work down to workers, not to throttle the workers
    /// themselves, so a worker sees the session as normal — including one
    /// already running when the user flips `/mode`. Every other mode
    /// propagates live: plan keeps gating children (a restriction that
    /// only held at depth 0 would not be a restriction), and yolo
    /// silences review for the whole tree.
    pub fn for_worker(mode: ModeHandle, plan: PlanArea) -> Self {
        Self {
            mode,
            plan,
            worker: true,
        }
    }

    /// The mode this gate enforces right now, after the worker view.
    fn stance(&self) -> Mode {
        let mode = self.mode.get();
        if self.worker {
            mode.child_mode()
        } else {
            mode
        }
    }
}

#[async_trait]
impl Extension for PlanGate {
    fn name(&self) -> &str {
        "plan-mode"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().before_tool().tool_result()
    }

    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        match self.stance() {
            Mode::Normal | Mode::Auto | Mode::Yolo => return Ok(ToolDecision::Continue),
            Mode::Plan => {
                if READ_ONLY_TOOLS.contains(&call.name.as_str()) {
                    return Ok(ToolDecision::Continue);
                }
            }
            Mode::Orchestrate => {
                if READ_ONLY_TOOLS.contains(&call.name.as_str())
                    || DELEGATION_TOOLS.contains(&call.name.as_str())
                    || matches!(
                        call.name.as_str(),
                        "write_file" | "edit_file" | "multi_edit" | "apply_patch"
                    )
                {
                    return Ok(ToolDecision::Continue);
                }
            }
        }
        if plan_paths::written_paths(call).is_some() {
            return Ok(ToolDecision::Continue);
        }
        Ok(ToolDecision::Deny {
            reason: if self.stance() == Mode::Orchestrate {
                format!(
                    "Orchestrate mode is on, so `{}` was not run — the parent may make basic \
                     native file edits, but delegates execution and substantial work. Hand \
                     this work to a worker with the `subagent` tool: one bounded, \
                     self-contained task with the exact result expected. Do not retry this \
                     call yourself; the user leaves orchestrate mode with /mode.",
                    call.name
                )
            } else {
                format!(
                    "Plan mode is on, so `{}` was not run — nothing may change the machine, \
                     and `{}/` is the only writable directory. Investigate with read_file, \
                     list_dir, grep, and glob, and answer with what you found. If the work \
                     warrants a plan, write it to a markdown file in `{}/`. Do not retry \
                     this call; the user leaves plan mode with /mode.",
                    call.name,
                    crate::plan::PLAN_DIR,
                    crate::plan::PLAN_DIR,
                )
            },
        })
    }

    /// Record plans the agent actually wrote, so leaving plan mode can
    /// report real files. Taken from the result rather than the call: a
    /// write the user denied or that failed is not a saved plan.
    async fn tool_result(&self, result: &orca_harness_core::ToolResult) {
        if result.is_error || !self.mode.is_plan() {
            return;
        }
        for path in plan_paths::result_paths(result) {
            self.plan.record(&path);
        }
    }
}
