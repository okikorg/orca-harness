//! Built-in mutable tool state (todos, the read-before-write guard, the
//! process manager) belongs to the session, not the agent: two sessions
//! opened from one agent must not see or control each other's state, while
//! state must persist across turns inside one session.

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::ModelResponse;
use orca_harness_sdk::{Harness, TodoList, ToolPreset};
use serde_json::json;

mod common;
use common::temp_dir;

fn todo_call(id: &str) -> orca_harness_core::ToolCall {
    call(
        id,
        "todo_write",
        json!({"todos": [{"content": "ship it", "status": "in_progress"}]}),
    )
}

#[tokio::test]
async fn todos_are_isolated_between_sessions() {
    let root = temp_dir("todos-isolated");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let model = ScriptedModel::tool_round(vec![todo_call("t1")], "planned");
    let agent = harness.agent(model).todos().build().unwrap();

    let a = agent.new_session().ephemeral().open().unwrap();
    let b = agent.new_session().ephemeral().open().unwrap();
    assert_eq!(a.run("plan").await.unwrap().text, "planned");

    assert_eq!(a.todo_list().unwrap().items().len(), 1);
    assert!(b.todo_list().unwrap().items().is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn todos_shared_when_explicitly_configured() {
    let root = temp_dir("todos-shared");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let model = ScriptedModel::tool_round(vec![todo_call("t1")], "planned");
    let list = TodoList::new();
    let agent = harness
        .agent(model)
        .todos_shared(list.clone())
        .build()
        .unwrap();

    let a = agent.new_session().ephemeral().open().unwrap();
    let b = agent.new_session().ephemeral().open().unwrap();
    a.run("plan").await.unwrap();

    assert_eq!(list.items().len(), 1);
    assert_eq!(a.todo_list().unwrap().items().len(), 1);
    assert_eq!(b.todo_list().unwrap().items().len(), 1);
    #[allow(deprecated)]
    let agent_level = agent.todo_list();
    assert_eq!(agent_level.unwrap().items().len(), 1);

    let _ = std::fs::remove_dir_all(&root);
}

/// Scripts `read_file` for the first run, then `write_file` for the next
/// two. Sessions share the agent's model, so run order decides who gets
/// which round.
fn guard_model(path: &str) -> ScriptedModel {
    let write = |id: &str| {
        call(
            id,
            "write_file",
            json!({"path": path, "content": "overwritten"}),
        )
    };
    ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: None,
            calls: vec![call("r1", "read_file", json!({"path": path}))],
            usage: None,
        },
        ModelResponse::final_text("read"),
        ModelResponse::ToolCalls {
            content: None,
            calls: vec![write("w1")],
            usage: None,
        },
        ModelResponse::final_text("wrote"),
        ModelResponse::ToolCalls {
            content: None,
            calls: vec![write("w2")],
            usage: None,
        },
        ModelResponse::final_text("wrote again"),
    ])
}

#[tokio::test]
async fn file_guard_is_isolated_between_sessions() {
    let root = temp_dir("guard-isolated");
    std::fs::write(root.join("notes.txt"), "original").unwrap();
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(guard_model("notes.txt"))
        .tools(ToolPreset::ShellLess)
        .build()
        .unwrap();

    let a = agent.new_session().ephemeral().open().unwrap();
    let b = agent.new_session().ephemeral().open().unwrap();
    assert_eq!(a.run("read it").await.unwrap().text, "read");
    assert_eq!(a.file_guard().len(), 1);
    assert!(b.file_guard().is_empty());

    let rejected = b.run("overwrite it").await.unwrap();
    assert!(
        format!("{:?}", rejected.messages).contains("has not been read"),
        "session B never read the file, so its guard must reject the write"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("notes.txt")).unwrap(),
        "original"
    );
    assert!(b.file_guard().is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn multi_turn_state_persists_within_a_session() {
    let root = temp_dir("guard-multi-turn");
    std::fs::write(root.join("notes.txt"), "original").unwrap();
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(guard_model("notes.txt"))
        .tools(ToolPreset::ShellLess)
        .build()
        .unwrap();

    let session = agent.new_session().ephemeral().open().unwrap();
    assert_eq!(session.run("read it").await.unwrap().text, "read");
    let written = session.run("overwrite it").await.unwrap();
    assert_eq!(written.text, "wrote");
    assert!(
        !format!("{:?}", written.messages).contains("has not been read"),
        "the read in turn one must satisfy the guard in turn two"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("notes.txt")).unwrap(),
        "overwritten"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn fork_and_clear_reset_builtin_state() {
    let root = temp_dir("fork-clear-todos");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let model = ScriptedModel::tool_round(vec![todo_call("t1")], "planned");
    let agent = harness.agent(model).todos().build().unwrap();

    let session = agent.new_session().persistent().open().unwrap();
    session.run("plan").await.unwrap();
    assert_eq!(session.todo_list().unwrap().items().len(), 1);

    let fork = session.fork().await.unwrap();
    assert!(fork.todo_list().unwrap().items().is_empty());
    assert_eq!(session.todo_list().unwrap().items().len(), 1);

    session.clear().await.unwrap();
    assert!(session.todo_list().unwrap().items().is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn process_manager_is_per_session() {
    let root = temp_dir("process-per-session");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let model = ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: None,
            calls: vec![call(
                "p1",
                "process",
                json!({"action": "spawn", "command": "sleep 5"}),
            )],
            usage: None,
        },
        ModelResponse::final_text("spawned"),
        ModelResponse::ToolCalls {
            content: None,
            calls: vec![call("p2", "process", json!({"action": "list"}))],
            usage: None,
        },
        ModelResponse::final_text("listed"),
    ]);
    let agent = harness
        .agent(model)
        .tools(ToolPreset::Coding)
        .build()
        .unwrap();

    let a = agent.new_session().ephemeral().open().unwrap();
    let b = agent.new_session().ephemeral().open().unwrap();
    assert_eq!(a.run("start").await.unwrap().text, "spawned");
    let listed = b.run("list").await.unwrap();
    assert_eq!(listed.text, "listed");
    let tool_results: Vec<String> = listed
        .messages
        .iter()
        .map(|message| serde_json::to_string(message).unwrap())
        .collect();
    assert!(
        tool_results.iter().any(|m| m.contains("processes")),
        "list result missing: {tool_results:?}"
    );
    assert!(
        !tool_results.iter().any(|m| m.contains("sleep 5")),
        "session B must not see session A's process: {tool_results:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
