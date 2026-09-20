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
    assert_eq!(Mode::from_label("AUTOMATIC"), Some(Mode::Auto));
    assert_eq!(Mode::from_label("YOLO"), Some(Mode::Yolo));
    assert_eq!(Mode::from_label("nonsense"), None);
}

#[test]
fn handle_defaults_to_normal_and_shares_state() {
    let handle = ModeHandle::default();
    assert_eq!(handle.get(), Mode::Normal);
    assert!(!handle.is_plan());
    handle.set(Mode::Yolo);
    assert!(handle.get().bypasses_human_approval());
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
        "apply_patch",
        "multi_edit",
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

/// Native edits and delegation are allowed; unknown tools stay denied.
#[tokio::test]
async fn orchestrate_mode_allows_reads_edits_and_delegation() {
    let gate = PlanGate::new(ModeHandle::new(Mode::Orchestrate), PlanArea::new());
    for name in [
        "read_file",
        "list_dir",
        "grep",
        "glob",
        "web_fetch",
        "skill",
        "todo_write",
        "ask",
        "write_file",
        "edit_file",
        "apply_patch",
        "multi_edit",
        "subagent",
        "workflow",
    ] {
        let decision = gate.before_tool(&call(name)).await.unwrap();
        assert!(matches!(decision, ToolDecision::Continue), "{name}");
    }
    for name in [
        "shell",
        "process",
        "pykernel",
        "bun_repl",
        "memory_manage",
        "mcp__x__y",
        "some_future_tool",
    ] {
        assert!(
            denied(&gate.before_tool(&call(name)).await.unwrap()),
            "{name}"
        );
    }
}

#[tokio::test]
async fn orchestrate_accepts_source_edits_and_delegation_but_blocks_execution() {
    let mode = ModeHandle::new(Mode::Orchestrate);
    let gate = PlanGate::new(mode.clone(), PlanArea::new());
    for (name, arguments) in [
        (
            "write_file",
            json!({"path": "src/greeting.rs", "content": "pub const GREETING: &str = \"Hello\";\n"}),
        ),
        (
            "edit_file",
            json!({"path": "src/main.rs", "old": "Helo", "new": "Hello"}),
        ),
        (
            "multi_edit",
            json!({"edits": [{"path": "src/main.rs", "old": "Helo", "new": "Hello"}]}),
        ),
        (
            "apply_patch",
            json!({"patch": "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-// Helo\n+// Hello\n*** End Patch"}),
        ),
        (
            "subagent",
            json!({"task": "Implement greeting localization in src/greeting.rs, run focused tests, and report changed files and results."}),
        ),
        (
            "workflow",
            json!({"action": "run", "graph": [{"id": "tests", "prompt": "Run cargo test and report failures with their exact output."}]}),
        ),
    ] {
        let call = ToolCall {
            arguments,
            ..call(name)
        };
        assert!(
            matches!(
                gate.before_tool(&call).await.unwrap(),
                ToolDecision::Continue
            ),
            "{name}"
        );
        mode.set(Mode::Plan);
        assert!(denied(&gate.before_tool(&call).await.unwrap()), "{name}");
        mode.set(Mode::Orchestrate);
    }
    for (name, arguments) in [
        ("shell", json!({"command": "cargo test"})),
        (
            "process",
            json!({"action": "spawn", "command": "cargo test", "waitForExit": true}),
        ),
        ("pykernel", json!({"code": "print(1 + 1)"})),
        ("bun_repl", json!({"code": "console.log(1 + 1)"})),
        (
            "mcp__github__create_issue",
            json!({"owner": "example", "repo": "app", "title": "Fix greeting"}),
        ),
    ] {
        assert!(
            denied(
                &gate
                    .before_tool(&ToolCall {
                        arguments,
                        ..call(name)
                    })
                    .await
                    .unwrap()
            ),
            "{name}"
        );
    }
}

/// The denial hands the model the next move — delegate — rather than
/// a dead end, and names the tool that was refused.
#[tokio::test]
async fn orchestrate_denial_points_at_delegation() {
    let gate = PlanGate::new(ModeHandle::new(Mode::Orchestrate), PlanArea::new());
    let ToolDecision::Deny { reason } = gate
        .before_tool(&ToolCall {
            arguments: json!({"command": "cargo test"}),
            ..call("shell")
        })
        .await
        .unwrap()
    else {
        panic!("expected a denial");
    };
    assert!(reason.contains("shell"), "{reason}");
    assert!(reason.contains("subagent"), "{reason}");
    assert!(reason.contains("/mode"), "{reason}");
    assert!(!reason.contains("docs/plan/"), "{reason}");
}

/// Workers spawned by the orchestrator keep their full tools: the
/// restriction exists to push work down, not to throttle the workers
/// it pushed it to.
#[tokio::test]
async fn worker_gate_reads_orchestrate_as_normal() {
    let mode = ModeHandle::new(Mode::Orchestrate);
    let gate = PlanGate::for_worker(mode.clone(), PlanArea::new());
    for name in ["shell", "write_file", "subagent", "mcp__x__y"] {
        let decision = gate.before_tool(&call(name)).await.unwrap();
        assert!(matches!(decision, ToolDecision::Continue), "{name}");
    }

    // Live propagation survives: flipping the session to plan
    // restrains an in-flight worker on its very next call.
    mode.set(Mode::Plan);
    assert!(denied(&gate.before_tool(&call("shell")).await.unwrap()));
}

/// The top-level gate in orchestrate mode records nothing in the plan
/// area — only plan mode tracks plan artifacts.
#[tokio::test]
async fn orchestrate_records_no_plans() {
    let area = PlanArea::new();
    let mode = ModeHandle::new(Mode::Orchestrate);
    let gate = PlanGate::new(mode.clone(), area.clone());
    gate.tool_result(&orca_harness_core::ToolResult {
        call_id: "c1".into(),
        tool_name: "write_file".into(),
        output: serde_json::json!({
            "path": "docs/plan/x.md",
            "bytesWritten": 6
        }),
        is_error: false,
    })
    .await;
    assert!(area.written().is_empty());
    // And leaving orchestrate for normal reports nothing.
    mode.set(Mode::Normal);
    assert!(area.end().is_empty());
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
        "apply_patch",
        "multi_edit",
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
    let multi_edit = ToolCall {
        id: "c4".into(),
        name: "multi_edit".into(),
        arguments: json!({"edits": [
            {"path": "docs/plan/x.md", "old": "a", "new": "b"},
            {"path": "docs/plan/y.md", "old": "c", "new": "d"}
        ]}),
    };
    assert!(matches!(
        gate.before_tool(&multi_edit).await.unwrap(),
        ToolDecision::Continue
    ));
    let patch = ToolCall {
        id: "c5".into(),
        name: "apply_patch".into(),
        arguments: json!({"patch": "*** Begin Patch\n*** Update File: docs/plan/x.md\n@@\n-a\n+b\n*** Add File: docs/plan/z.md\n+# Plan\n*** End Patch"}),
    };
    assert!(matches!(
        gate.before_tool(&patch).await.unwrap(),
        ToolDecision::Continue
    ));

    // One path outside the fence denies the entire batch, and plan mode
    // never permits deletion through a patch.
    let mixed = ToolCall {
        id: "c6".into(),
        name: "multi_edit".into(),
        arguments: json!({"edits": [
            {"path": "docs/plan/x.md", "old": "a", "new": "b"},
            {"path": "src/main.rs", "old": "c", "new": "d"}
        ]}),
    };
    assert!(denied(&gate.before_tool(&mixed).await.unwrap()));
    let deleting_patch = ToolCall {
        id: "c7".into(),
        name: "apply_patch".into(),
        arguments: json!({"patch": "*** Begin Patch\n*** Delete File: docs/plan/x.md\n*** End Patch"}),
    };
    assert!(denied(&gate.before_tool(&deleting_patch).await.unwrap()));

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

    gate.tool_result(&orca_harness_core::ToolResult {
        call_id: "c2".into(),
        tool_name: "apply_patch".into(),
        output: json!({
            "paths": ["docs/plan/kept.md", "docs/plan/second.md"],
            "filesChanged": 2
        }),
        is_error: false,
    })
    .await;
    assert_eq!(
        area.written(),
        [
            "docs/plan/kept.md".to_string(),
            "docs/plan/second.md".to_string()
        ]
    );

    // Outside plan mode nothing is recorded at all.
    mode.set(Mode::Normal);
    gate.tool_result(&result("write_file", "docs/plan/later.md", false))
        .await;
    assert_eq!(area.written().len(), 2);
}

/// The denial has to send the model somewhere other than "try again".
#[tokio::test]
async fn denial_names_the_tool_and_asks_for_a_plan() {
    let gate = PlanGate::new(ModeHandle::new(Mode::Plan), PlanArea::new());
    let ToolDecision::Deny { reason } = gate.before_tool(&call("write_file")).await.unwrap() else {
        panic!("expected a denial");
    };
    assert!(reason.contains("write_file"));
    assert!(reason.contains("plan"));
    assert!(reason.contains("/mode"));
}
