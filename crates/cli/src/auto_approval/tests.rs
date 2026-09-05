use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use orca_harness_core::testing::{call as scripted_call, ScriptedModel};
use orca_harness_core::{Agent, FnTool, ModelError, ToolDecision};
use orca_harness_tools::{EditFileTool, ShellTool, Workspace};

#[test]
fn routine_local_work_skips_review_but_egress_and_deletion_do_not() {
    let call = |name: &str| ToolCall {
        id: "c1".into(),
        name: name.into(),
        arguments: json!({}),
    };
    let workspace_root = std::env::current_dir().unwrap();
    assert!(AutoApproval::known_safe(
        &call("read_file"),
        &json!({}),
        &workspace_root
    ));
    assert!(AutoApproval::known_safe(
        &call("process"),
        &json!({"action": "list"}),
        &workspace_root
    ));
    assert!(!AutoApproval::known_safe(
        &call("process"),
        &json!({"action": "spawn", "command": "true"}),
        &workspace_root
    ));
    assert!(!AutoApproval::known_safe(
        &call("web_fetch"),
        &json!({}),
        &workspace_root
    ));
    assert!(!AutoApproval::known_safe(
        &call("mcp__github__create_issue"),
        &json!({}),
        &workspace_root
    ));
    for name in ["write_file", "edit_file", "multi_edit"] {
        assert!(AutoApproval::known_safe(
            &call(name),
            &json!({}),
            &workspace_root
        ));
    }
    assert!(AutoApproval::known_safe(
        &call("apply_patch"),
        &json!({"patch": "*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** End Patch"}),
        &workspace_root
    ));
    assert!(!AutoApproval::known_safe(
        &call("apply_patch"),
        &json!({"patch": "*** Begin Patch\n*** Delete File: a.rs\n*** End Patch"}),
        &workspace_root
    ));
}

#[test]
fn reviewer_prompt_allows_normal_intermediate_development_steps() {
    for expected in [
        "reasonable step",
        "repository exploration",
        "project scripts",
        "intermediate",
        "meaningful safety or scope concern",
    ] {
        assert!(REVIEW_SYSTEM.contains(expected));
    }
    assert!(!REVIEW_SYSTEM.contains("clearly necessary"));
}

#[tokio::test]
async fn real_workspace_edit_bypasses_the_reviewer() {
    let root = std::env::temp_dir().join(format!("orca-auto-routine-edit-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("demo.txt");
    std::fs::write(&path, "before\n").unwrap();

    let packets = Arc::new(Mutex::new(Vec::new()));
    let reviewer: Arc<dyn Model> = Arc::new(RecordingReviewer {
        response: r#"{"decision":"caution","reason":"should not be called"}"#,
        packets: packets.clone(),
    });
    let model = ScriptedModel::tool_round(
        vec![scripted_call(
            "edit",
            "edit_file",
            json!({"path": "demo.txt", "old": "before", "new": "after"}),
        )],
        "done",
    );

    let answer = Agent::new(model)
        .tool(EditFileTool::new(Workspace::new(root.clone())))
        .extension(AutoApproval::new(
            ModeHandle::new(Mode::Auto),
            reviewer,
            &root,
        ))
        .run("update demo.txt")
        .await
        .unwrap();

    assert_eq!(answer, "done");
    assert!(packets.lock().unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "after\n");
    std::fs::remove_dir_all(root).ok();
}

#[tokio::test]
async fn real_routine_shell_command_bypasses_the_reviewer() {
    let root = std::env::temp_dir().join(format!("orca-auto-routine-shell-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();

    let packets = Arc::new(Mutex::new(Vec::new()));
    let reviewer: Arc<dyn Model> = Arc::new(RecordingReviewer {
        response: r#"{"decision":"caution","reason":"should not be called"}"#,
        packets: packets.clone(),
    });
    let model = ScriptedModel::tool_round(
        vec![scripted_call("shell", "shell", json!({"command": "true"}))],
        "done",
    );

    let answer = Agent::new(model)
        .tool(ShellTool::local().working_dir(root.to_string_lossy()))
        .extension(AutoApproval::new(
            ModeHandle::new(Mode::Auto),
            reviewer,
            &root,
        ))
        .run("run the routine check")
        .await
        .unwrap();

    assert_eq!(answer, "done");
    assert!(packets.lock().unwrap().is_empty());
    std::fs::remove_dir_all(root).ok();
}

#[tokio::test]
async fn composed_shell_command_falls_back_to_exact_action_review() {
    let root =
        std::env::temp_dir().join(format!("orca-auto-reviewed-shell-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();

    let packets = Arc::new(Mutex::new(Vec::new()));
    let reviewer: Arc<dyn Model> = Arc::new(RecordingReviewer {
        response: r#"{"decision":"clear","reason":"requested"}"#,
        packets: packets.clone(),
    });
    let model = ScriptedModel::tool_round(
        vec![scripted_call(
            "shell",
            "shell",
            json!({"command": "true && true"}),
        )],
        "done",
    );

    let answer = Agent::new(model)
        .tool(ShellTool::local().working_dir(root.to_string_lossy()))
        .extension(AutoApproval::new(
            ModeHandle::new(Mode::Auto),
            reviewer,
            &root,
        ))
        .run("run both checks")
        .await
        .unwrap();

    assert_eq!(answer, "done");
    let packets = packets.lock().unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0]["tool_name"], "shell");
    assert_eq!(packets[0]["arguments"], json!({"command": "true && true"}));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn reviewer_accepts_only_clear_or_explained_caution() {
    assert!(matches!(
        parse_review(ModelResponse::final_text(
            r#"{"decision":"clear","reason":"within scope"}"#
        )),
        Ok(ReviewDecision::Clear)
    ));
    assert!(matches!(
        parse_review(ModelResponse::final_text(
            r#"{"decision":"caution","reason":"too broad"}"#
        )),
        Ok(ReviewDecision::Caution(reason)) if reason == "too broad"
    ));
    assert!(parse_review(ModelResponse::final_text(
        r#"{"decision":"allow","reason":"invented"}"#
    ))
    .is_err());
}

struct OneCallModel(AtomicUsize);

#[async_trait]
impl Model for OneCallModel {
    async fn generate(
        &self,
        context: &Context,
        _tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        if context
            .messages()
            .iter()
            .any(|message| matches!(message, Message::Tool { .. }))
        {
            return Ok(ModelResponse::final_text("done"));
        }
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(ModelResponse::tool_calls(vec![ToolCall {
            id: "pending".into(),
            name: "mutate".into(),
            arguments: json!({"path": "before.txt"}),
        }]))
    }
}

struct RecordingReviewer {
    response: &'static str,
    packets: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl Model for RecordingReviewer {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "permission_decision");
        let packet = context.messages().iter().find_map(|message| match message {
            Message::User { content, .. } => serde_json::from_str(content).ok(),
            _ => None,
        });
        self.packets.lock().unwrap().push(packet.unwrap());
        Ok(ModelResponse::final_text(self.response))
    }
}

struct Rewrite;

#[async_trait]
impl Extension for Rewrite {
    fn name(&self) -> &str {
        "rewrite"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().before_tool()
    }

    async fn before_tool(&self, _call: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        Ok(ToolDecision::Rewrite(json!({"path": "after.txt"})))
    }
}

#[tokio::test]
async fn auto_reviews_and_executes_the_final_rewritten_action() {
    let packets = Arc::new(Mutex::new(Vec::new()));
    let reviewer: Arc<dyn Model> = Arc::new(RecordingReviewer {
        response: r#"{"decision":"clear","reason":"requested"}"#,
        packets: packets.clone(),
    });
    let executed = Arc::new(Mutex::new(None));
    let recorded = executed.clone();
    let auto = AutoApproval::new(ModeHandle::new(Mode::Auto), reviewer, std::env::temp_dir());
    let agent = Agent::new(OneCallModel(AtomicUsize::new(0)))
        .extension(Rewrite)
        .extension(auto)
        .tool(FnTool::new(
            "mutate",
            "test mutation",
            json!({"type": "object"}),
            move |input, _ctx| {
                *recorded.lock().unwrap() = Some(input);
                async { Ok(json!({"ok": true})) }
            },
        ));

    assert_eq!(
        agent.run("change the requested file").await.unwrap(),
        "done"
    );
    assert_eq!(
        executed.lock().unwrap().as_ref(),
        Some(&json!({"path": "after.txt"}))
    );
    let packets = packets.lock().unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0]["root_request"], "change the requested file");
    assert_eq!(packets[0]["arguments"], json!({"path": "after.txt"}));
}

#[tokio::test]
async fn invalid_review_fails_closed_without_executing() {
    let reviewer: Arc<dyn Model> = Arc::new(RecordingReviewer {
        response: "not a decision",
        packets: Arc::default(),
    });
    let executed = Arc::new(AtomicBool::new(false));
    let flag = executed.clone();
    let agent = Agent::new(OneCallModel(AtomicUsize::new(0)))
        .extension(AutoApproval::new(
            ModeHandle::new(Mode::Auto),
            reviewer,
            std::env::temp_dir(),
        ))
        .tool(FnTool::new(
            "mutate",
            "test mutation",
            json!({"type": "object"}),
            move |_input, _ctx| {
                flag.store(true, Ordering::Relaxed);
                async { Ok(json!({"ok": true})) }
            },
        ));

    assert_eq!(agent.run("change the file").await.unwrap(), "done");
    assert!(!executed.load(Ordering::Relaxed));
}

#[tokio::test]
async fn exact_cautions_are_reused_for_the_current_root_request() {
    let packets = Arc::new(Mutex::new(Vec::new()));
    let reviewer: Arc<dyn Model> = Arc::new(RecordingReviewer {
        response: r#"{"decision":"caution","reason":"too broad"}"#,
        packets: packets.clone(),
    });
    let auto = AutoApproval::new(ModeHandle::new(Mode::Auto), reviewer, std::env::temp_dir());
    let mut context = Context::new();
    context.push_user("make the scoped change");
    auto.capture_request(&context);
    let call = ToolCall {
        id: "pending".into(),
        name: "mutate".into(),
        arguments: json!({}),
    };
    let input = json!({"path": "broad-target"});

    assert!(matches!(
        auto.review(&call, &input).await.unwrap(),
        ReviewDecision::Caution(reason) if reason == "too broad"
    ));
    assert!(matches!(
        auto.review(&call, &input).await.unwrap(),
        ReviewDecision::Caution(reason) if reason == "too broad"
    ));
    assert_eq!(packets.lock().unwrap().len(), 1);
}

#[test]
fn subagent_tasks_cannot_replace_root_authority() {
    let reviewer: Arc<dyn Model> = Arc::new(RecordingReviewer {
        response: r#"{"decision":"clear","reason":"requested"}"#,
        packets: Arc::default(),
    });
    let root = AutoApproval::new(ModeHandle::new(Mode::Auto), reviewer, std::env::temp_dir());
    let mut root_context = Context::new();
    root_context.push_user("root request");
    root.capture_request(&root_context);

    let child = root.for_subagent();
    let mut child_context = Context::new();
    child_context.push_user("broader delegated task");
    child.capture_request(&child_context);

    assert_eq!(root.authority.lock().unwrap().root_request, "root request");
}
