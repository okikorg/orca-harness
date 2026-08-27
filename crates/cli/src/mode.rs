//! Session mode: the stance a run takes toward acting on the machine.
//!
//! `Normal` is the agent as usual — every registered tool is available,
//! and the gated ones ask for approval. `Plan` is read-only: the agent
//! may investigate but may not change anything, so it answers with a
//! plan instead of a diff. `Yolo` removes friction instead of adding
//! it: gated tools stop asking entirely. Because that silences exactly
//! the mechanism whose job is to say "wait", it never renders quietly —
//! the status line carries `yolo` for as long as it is on.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    /// Read-only: only the tools in [`READ_ONLY_TOOLS`] run.
    Plan,
    /// Every tool runs without approval prompts. The status line shows
    /// `yolo` for the whole session.
    Yolo,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Normal, Mode::Plan, Mode::Yolo];

    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Plan => "plan",
            Mode::Yolo => "yolo",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Mode::Normal => "every tool is available; gated tools ask for approval",
            Mode::Plan => "read-only: the agent investigates and proposes, but changes nothing",
            Mode::Yolo => "every tool runs without approval prompts",
        }
    }

    /// Parse a label as produced by [`Mode::label`], accepting the
    /// spellings a user is likely to type.
    pub fn from_label(label: &str) -> Option<Mode> {
        match label.trim().to_ascii_lowercase().as_str() {
            "normal" | "default" | "off" | "act" => Some(Mode::Normal),
            "plan" | "planning" | "read-only" | "readonly" | "ro" => Some(Mode::Plan),
            "yolo" => Some(Mode::Yolo),
            _ => None,
        }
    }

    /// True when gated tools must not prompt. Plan mode denies them
    /// before approval is ever consulted ([`PlanGate`] registers
    /// first); yolo skips the ask on purpose.
    pub fn bypasses_approval(self) -> bool {
        matches!(self, Mode::Yolo)
    }
}

/// The tools that only observe: they read files, search, or fetch, and
/// leave the machine exactly as they found it. Everything else — `shell`,
/// `process`, `pykernel`, `bun_repl`, `write_file`, `edit_file`, `subagent`, the
/// `fs_admin` bundle, every MCP tool — is denied in plan mode.
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
            _ => Mode::Yolo,
        }
    }

    pub fn set(&self, mode: Mode) {
        let encoded = match mode {
            Mode::Normal => 0,
            Mode::Plan => 1,
            Mode::Yolo => 2,
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
/// `write_file` and `edit_file` are allowed against markdown files in
/// `docs/plan/` — and nowhere else. Which file, and whether to write one
/// at all, is the agent's decision; the gate only holds the fence.
/// Because it runs before approval, an "always allow write_file" grant
/// cannot widen past that directory.
pub struct PlanGate {
    mode: ModeHandle,
    plan: PlanArea,
}

impl PlanGate {
    pub fn new(mode: ModeHandle, plan: PlanArea) -> Self {
        Self { mode, plan }
    }

    /// The path a write-shaped call targets, if it has one.
    fn written_path(call_name: &str, path: Option<&str>) -> Option<String> {
        matches!(call_name, "write_file" | "edit_file")
            .then(|| path.filter(|p| crate::plan::is_plan_path(p)))
            .flatten()
            .map(str::to_string)
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
        if !self.mode.is_plan() || READ_ONLY_TOOLS.contains(&call.name.as_str()) {
            return Ok(ToolDecision::Continue);
        }
        let path = call.arguments.get("path").and_then(|path| path.as_str());
        if Self::written_path(&call.name, path).is_some() {
            return Ok(ToolDecision::Continue);
        }
        Ok(ToolDecision::Deny {
            reason: format!(
                "Plan mode is on, so `{}` was not run — nothing may change the machine, and \
                 `{}/` is the only writable directory. Investigate with read_file, list_dir, \
                 grep, and glob, and answer with what you found. If the work warrants a plan, \
                 write it to a markdown file in `{}/`. Do not retry this call; the user \
                 leaves plan mode with /mode.",
                call.name,
                crate::plan::PLAN_DIR,
                crate::plan::PLAN_DIR,
            ),
        })
    }

    /// Record plans the agent actually wrote, so leaving plan mode can
    /// report real files. Taken from the result rather than the call: a
    /// write the user denied or that failed is not a saved plan.
    async fn tool_result(&self, result: &orca_harness_core::ToolResult) {
        if result.is_error || !self.mode.is_plan() {
            return;
        }
        let path = result.output.get("path").and_then(|path| path.as_str());
        if let Some(path) = Self::written_path(&result.tool_name, path) {
            self.plan.record(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: json!({}),
        }
    }

    fn denied(decision: &ToolDecision) -> bool {
        matches!(decision, ToolDecision::Deny { .. })
    }

    #[test]
    fn labels_round_trip_and_accept_synonyms() {
        for mode in Mode::ALL {
            assert_eq!(Mode::from_label(mode.label()), Some(mode));
        }
        assert_eq!(Mode::from_label("READ-ONLY"), Some(Mode::Plan));
        assert_eq!(Mode::from_label(" off "), Some(Mode::Normal));
        assert_eq!(Mode::from_label("YOLO"), Some(Mode::Yolo));
        assert_eq!(Mode::from_label("nonsense"), None);
    }

    #[test]
    fn handle_defaults_to_normal_and_shares_state() {
        let handle = ModeHandle::default();
        assert_eq!(handle.get(), Mode::Normal);
        assert!(!handle.is_plan());
        handle.set(Mode::Yolo);
        assert!(handle.get().bypasses_approval());
        // Clones share one state: the TUI and the gate see the same mode.
        let clone = handle.clone();
        handle.set(Mode::Plan);
        assert!(clone.is_plan());
    }

    #[tokio::test]
    async fn normal_mode_gates_nothing() {
        let gate = PlanGate::new(ModeHandle::new(Mode::Normal), PlanArea::new());
        for name in ["shell", "write_file", "read_file", "mcp__x__y"] {
            let decision = gate.before_tool(&call(name)).await.unwrap();
            assert!(matches!(decision, ToolDecision::Continue), "{name}");
        }
    }

    #[tokio::test]
    async fn plan_mode_allows_only_the_read_only_set() {
        let gate = PlanGate::new(ModeHandle::new(Mode::Plan), PlanArea::new());
        for name in READ_ONLY_TOOLS {
            let decision = gate.before_tool(&call(name)).await.unwrap();
            assert!(matches!(decision, ToolDecision::Continue), "{name}");
        }
        for name in [
            "shell",
            "process",
            "pykernel",
            "bun_repl",
            "write_file",
            "edit_file",
            "subagent",
            "memory_manage",
            "delete_file",
        ] {
            assert!(
                denied(&gate.before_tool(&call(name)).await.unwrap()),
                "{name}"
            );
        }
    }

    /// The allowlist is the whole point: a tool this file has never heard
    /// of — an MCP server's, a future core tool — must be denied, not
    /// allowed by omission.
    #[tokio::test]
    async fn unknown_tools_are_denied_in_plan_mode() {
        let gate = PlanGate::new(ModeHandle::new(Mode::Plan), PlanArea::new());
        assert!(denied(
            &gate
                .before_tool(&call("mcp__github__create_pr"))
                .await
                .unwrap()
        ));
        assert!(denied(
            &gate.before_tool(&call("some_future_tool")).await.unwrap()
        ));
    }

    /// Yolo is not plan mode: the gate stays out of the way and every
    /// tool — known or unknown — runs.
    #[tokio::test]
    async fn yolo_mode_gates_nothing() {
        let gate = PlanGate::new(ModeHandle::new(Mode::Yolo), PlanArea::new());
        for name in [
            "shell",
            "write_file",
            "edit_file",
            "process",
            "pykernel",
            "bun_repl",
            "subagent",
            "memory_manage",
            "mcp__x__y",
            "some_future_tool",
        ] {
            let decision = gate.before_tool(&call(name)).await.unwrap();
            assert!(matches!(decision, ToolDecision::Continue), "{name}");
        }
    }

    /// The one exception is the plan area: in yolo the whole machine is
    /// writable anyway, so no path is special and none is recorded as
    /// "a plan" — that vocabulary belongs to plan mode.
    #[tokio::test]
    async fn yolo_mode_records_no_plans() {
        let area = PlanArea::new();
        let mode = ModeHandle::new(Mode::Yolo);
        let gate = PlanGate::new(mode.clone(), area.clone());
        let write = ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            arguments: json!({"path": "docs/plan/x.md", "content": "# Plan"}),
        };
        assert!(matches!(
            gate.before_tool(&write).await.unwrap(),
            ToolDecision::Continue
        ));
        gate.tool_result(&orca_harness_core::ToolResult {
            call_id: "c1".into(),
            tool_name: "write_file".into(),
            output: json!({"path": "docs/plan/x.md", "bytesWritten": 6}),
            is_error: false,
        })
        .await;
        assert!(area.written().is_empty());
        // And leaving yolo for normal reports nothing.
        mode.set(Mode::Normal);
        assert!(area.end().is_empty());
    }

    /// Flipping the handle applies to the very next call, with no agent
    /// rebuild in between.
    #[tokio::test]
    async fn switching_the_handle_applies_immediately() {
        let mode = ModeHandle::new(Mode::Normal);
        let gate = PlanGate::new(mode.clone(), PlanArea::new());
        assert!(matches!(
            gate.before_tool(&call("shell")).await.unwrap(),
            ToolDecision::Continue
        ));
        mode.set(Mode::Plan);
        assert!(denied(&gate.before_tool(&call("shell")).await.unwrap()));
        mode.set(Mode::Normal);
        assert!(matches!(
            gate.before_tool(&call("shell")).await.unwrap(),
            ToolDecision::Continue
        ));
    }

    /// The plan area is the one writable place, and the agent picks the
    /// filename inside it — the gate holds the fence, not the naming.
    #[tokio::test]
    async fn any_plan_file_is_writable_and_nothing_outside_is() {
        let gate = PlanGate::new(ModeHandle::new(Mode::Plan), PlanArea::new());
        let write = |path: &str| ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            arguments: json!({"path": path, "content": "# Plan"}),
        };

        // Whatever the agent decides to call it.
        for path in [
            "docs/plan/2026-08-22-add-rewind.md",
            "docs/plan/refactor-the-dispatcher.md",
            "./docs/plan/anything.md",
        ] {
            let decision = gate.before_tool(&write(path)).await.unwrap();
            assert!(matches!(decision, ToolDecision::Continue), "{path}");
        }
        // Revising one is the same permission.
        let edit = ToolCall {
            id: "c2".into(),
            name: "edit_file".into(),
            arguments: json!({"path": "docs/plan/x.md", "old": "a", "new": "b"}),
        };
        assert!(matches!(
            gate.before_tool(&edit).await.unwrap(),
            ToolDecision::Continue
        ));

        // Everything outside the fence stays refused.
        for path in [
            "src/main.rs",
            "docs/design.html",
            "docs/plan/../../src/main.rs",
            "docs/plan/sub/nested.md",
            "docs/plan/notes.txt",
            "docs/plan",
            "",
        ] {
            assert!(
                denied(&gate.before_tool(&write(path)).await.unwrap()),
                "{path}"
            );
        }
        // A write with no path argument is not a plan.
        let pathless = ToolCall {
            id: "c3".into(),
            name: "write_file".into(),
            arguments: json!({"content": "x"}),
        };
        assert!(denied(&gate.before_tool(&pathless).await.unwrap()));
    }

    /// The refusal points at the directory and leaves the decision with
    /// the model — it must not imply a plan file is expected.
    #[tokio::test]
    async fn the_denial_points_at_the_directory_not_a_file() {
        let gate = PlanGate::new(ModeHandle::new(Mode::Plan), PlanArea::new());
        let ToolDecision::Deny { reason } = gate.before_tool(&call("shell")).await.unwrap() else {
            panic!("expected a denial");
        };
        assert!(reason.contains("docs/plan/"), "{reason}");
        assert!(reason.contains("If the work warrants a plan"), "{reason}");
        assert!(reason.contains("/mode"), "{reason}");
    }

    /// What the episode reports must be what reached disk: successful
    /// plan writes only, never a denial, a failure, or a normal-mode
    /// write that happens to land in the directory.
    #[tokio::test]
    async fn only_successful_plan_writes_are_recorded() {
        let area = PlanArea::new();
        let mode = ModeHandle::new(Mode::Plan);
        let gate = PlanGate::new(mode.clone(), area.clone());

        let result = |name: &str, path: &str, is_error: bool| orca_harness_core::ToolResult {
            call_id: "c1".into(),
            tool_name: name.into(),
            output: json!({"path": path, "bytesWritten": 12}),
            is_error,
        };

        gate.tool_result(&result("write_file", "docs/plan/kept.md", false))
            .await;
        // Failures and denials are not saved plans.
        gate.tool_result(&result("write_file", "docs/plan/failed.md", true))
            .await;
        // Nor is a write outside the area, or a read of one.
        gate.tool_result(&result("write_file", "src/main.rs", false))
            .await;
        gate.tool_result(&result("read_file", "docs/plan/kept.md", false))
            .await;
        assert_eq!(area.written(), ["docs/plan/kept.md".to_string()]);

        // An edit of the plan counts as the same artifact, once.
        gate.tool_result(&result("edit_file", "docs/plan/kept.md", false))
            .await;
        assert_eq!(area.written().len(), 1);

        // Outside plan mode nothing is recorded at all.
        mode.set(Mode::Normal);
        gate.tool_result(&result("write_file", "docs/plan/later.md", false))
            .await;
        assert_eq!(area.written().len(), 1);
    }

    /// The denial has to send the model somewhere other than "try again".
    #[tokio::test]
    async fn denial_names_the_tool_and_asks_for_a_plan() {
        let gate = PlanGate::new(ModeHandle::new(Mode::Plan), PlanArea::new());
        let ToolDecision::Deny { reason } = gate.before_tool(&call("write_file")).await.unwrap()
        else {
            panic!("expected a denial");
        };
        assert!(reason.contains("write_file"));
        assert!(reason.contains("plan"));
        assert!(reason.contains("/mode"));
    }
}
