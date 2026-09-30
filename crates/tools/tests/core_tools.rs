use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use orca_harness_core::testing::call;
use orca_harness_core::{
    Agent, CancellationToken, Concurrency, Dispatcher, Extension, ExtensionRegistry, Next,
    Subscriptions, Tool, ToolCall, ToolContext, ToolError, ToolRegistry,
};
use orca_harness_tools::{EditFileTool, FileGuard, MutationPreflight, Workspace};
use serde_json::{json, Value};

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_ws() -> (Workspace, std::path::PathBuf) {
    let sequence = TEMP_SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "orca-harness-core-tools-{}-{sequence}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    (Workspace::new(dir.clone()), dir)
}

fn ctx(name: &str) -> ToolContext {
    ToolContext {
        call_id: "test".into(),
        tool_name: name.into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

struct ReviewRecorder(Arc<AtomicBool>);

#[async_trait]
impl Extension for ReviewRecorder {
    fn name(&self) -> &str {
        "review-recorder"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().around_tool()
    }

    async fn around_tool<'a>(
        &self,
        _call: &ToolCall,
        input: Value,
        _ctx: &ToolContext,
        next: Next<'a>,
    ) -> Result<Value, ToolError> {
        self.0.store(true, Ordering::SeqCst);
        next.run(input).await
    }
}

#[tokio::test]
async fn malformed_edit_is_rejected_before_later_review_extensions() {
    let (ws, dir) = temp_ws();
    let reviewed = Arc::new(AtomicBool::new(false));
    let model = orca_harness_core::testing::ScriptedModel::tool_round(
        vec![call(
            "invalid",
            "edit_file",
            json!({"edits": [{"path": "a.md", "old": "", "new": "notice"}]}),
        )],
        "done",
    );

    let answer = Agent::new(model)
        .tool(EditFileTool::new(ws))
        .extension(MutationPreflight)
        .extension(ReviewRecorder(reviewed.clone()))
        .run("append a notice")
        .await
        .unwrap();

    assert_eq!(answer, "done");
    assert!(!reviewed.load(Ordering::SeqCst));
    assert!(!dir.join("a.md").exists());
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn edit_file_applies_ordered_cross_file_replacements() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("a.txt"), "one one\n").unwrap();
    std::fs::write(dir.join("b.txt"), "alpha\n").unwrap();
    let guard = FileGuard::new();
    let tool = EditFileTool::new(ws).guard(guard.clone());

    let output = tool
        .call(
            json!({"edits": [
                {"path": "a.txt", "old": "one", "new": "two", "replaceAll": true},
                {"path": "a.txt", "old": "two two", "new": "three"},
                {"path": "b.txt", "old": "alpha", "new": "beta"}
            ]}),
            &ctx("edit_file"),
        )
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "three\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).unwrap(),
        "beta\n"
    );
    assert_eq!(output["editsApplied"], 3);
    assert_eq!(output["filesChanged"], 2);
    assert_eq!(output["replacements"], 4);
    assert_eq!(guard.len(), 2);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn edit_file_appends_to_multiple_files_atomically() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("a.md"), "# A\n").unwrap();
    std::fs::write(dir.join("b.md"), "# B\n").unwrap();
    let tool = EditFileTool::new(ws);

    let output = tool
        .call(
            json!({"edits": [
                {"path": "a.md", "operation": "append", "content": "\nnotice a\n"},
                {"path": "b.md", "operation": "append", "content": "\nnotice b\n"}
            ]}),
            &ctx("edit_file"),
        )
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.join("a.md")).unwrap(),
        "# A\n\nnotice a\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("b.md")).unwrap(),
        "# B\n\nnotice b\n"
    );
    assert_eq!(output["editsApplied"], 2);
    assert_eq!(output["filesChanged"], 2);
    assert_eq!(output["appends"], 2);
    assert_eq!(output["replacements"], 0);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn edit_file_append_preflight_failure_writes_nothing() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("a.md"), "stable\n").unwrap();
    let tool = EditFileTool::new(ws);

    let error = tool
        .call(
            json!({"edits": [
                {"path": "a.md", "operation": "append", "content": "changed\n"},
                {"path": "missing.md", "operation": "append", "content": "new\n"}
            ]}),
            &ctx("edit_file"),
        )
        .await
        .unwrap_err();

    assert!(error.message.contains("missing.md"));
    assert_eq!(
        std::fs::read_to_string(dir.join("a.md")).unwrap(),
        "stable\n"
    );
    assert!(!dir.join("missing.md").exists());
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn edit_file_empty_replacement_points_to_append_operation() {
    let (ws, dir) = temp_ws();
    let tool = EditFileTool::new(ws);
    let error = tool
        .call(
            json!({"edits": [{"path": "a.md", "old": "", "new": "notice"}]}),
            &ctx("edit_file"),
        )
        .await
        .unwrap_err();

    assert!(error.message.contains("operation: \"append\""));
    assert!(error.message.contains("write_file"));
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn single_edit_replaces_once_and_names_top_level_fields() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("a.txt"), "x y x\n").unwrap();
    let tool = EditFileTool::new(ws);

    let error = tool
        .call(
            json!({"path": "a.txt", "old": "x", "new": "z"}),
            &ctx("edit_file"),
        )
        .await
        .unwrap_err();
    assert!(
        error.message.contains("occurs 2 times"),
        "{}",
        error.message
    );
    let error = tool
        .call(json!({"path": "a.txt", "new": "z"}), &ctx("edit_file"))
        .await
        .unwrap_err();
    assert!(error.message.starts_with("`old`"), "{}", error.message);
    let error = tool.call(json!({}), &ctx("edit_file")).await.unwrap_err();
    assert!(error.message.contains("`edits`"), "{}", error.message);

    let output = tool
        .call(
            json!({"path": "a.txt", "old": "y", "new": "w"}),
            &ctx("edit_file"),
        )
        .await
        .unwrap();
    assert_eq!(output["editsApplied"], 1);
    assert_eq!(output["replacements"], 1);
    assert_eq!(output["paths"], json!(["a.txt"]));
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "x w x\n"
    );
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn edit_file_preflight_failure_writes_nothing() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("a.txt"), "before\n").unwrap();
    std::fs::write(dir.join("b.txt"), "stable\n").unwrap();
    let tool = EditFileTool::new(ws);

    let error = tool
        .call(
            json!({"edits": [
                {"path": "a.txt", "old": "before", "new": "after"},
                {"path": "b.txt", "old": "missing", "new": "changed"}
            ]}),
            &ctx("edit_file"),
        )
        .await
        .unwrap_err();

    assert!(error.message.contains("edit 1"));
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "before\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).unwrap(),
        "stable\n"
    );
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn edit_file_locks_every_distinct_path() {
    let (ws, dir) = temp_ws();
    let tool = EditFileTool::new(ws);
    assert_eq!(
        tool.concurrency(&json!({"edits": [
            {"path": "b.txt", "old": "b", "new": "B"},
            {"path": "a.txt", "old": "a", "new": "A"},
            {"path": "b.txt", "operation": "append", "content": "C"}
        ]})),
        Concurrency::Keys(vec!["file:a.txt".into(), "file:b.txt".into()])
    );
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn mutation_tools_normalize_aliases_to_the_same_concurrency_key() {
    let (ws, dir) = temp_ws();
    let tool = EditFileTool::new(ws);
    assert_eq!(
        tool.concurrency(&json!({"edits": [
            {"path": "./nested/../a.txt", "old": "a", "new": "A"},
            {"path": "a.txt", "old": "A", "new": "B"}
        ]})),
        Concurrency::Keys(vec!["file:a.txt".into()])
    );
    assert_eq!(
        tool.concurrency(&json!({"path": "./nested/../a.txt", "old": "a", "new": "A"})),
        Concurrency::Keys(vec!["file:a.txt".into()])
    );
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn aliases_have_safe_same_call_semantics() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();

    let tool = EditFileTool::new(ws.clone());
    tool.call(
        json!({"edits": [
            {"path": "./nested/../a.txt", "old": "a", "new": "b"},
            {"path": "a.txt", "old": "b", "new": "c"}
        ]}),
        &ctx("edit_file"),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "c\n");
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn no_op_edits_do_not_write_or_restamp_files() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("same.txt"), "same\n").unwrap();
    let guard = FileGuard::new();

    let tool = EditFileTool::new(ws).guard(guard.clone());
    let output = tool
        .call(
            json!({"path": "same.txt", "old": "same", "new": "same"}),
            &ctx("edit_file"),
        )
        .await
        .unwrap();
    assert_eq!(output["filesChanged"], 0);
    assert_eq!(output["replacements"], 1);
    assert_eq!(guard.len(), 0);
    assert_eq!(
        std::fs::read_to_string(dir.join("same.txt")).unwrap(),
        "same\n"
    );
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_path_calls_serialize_in_model_order() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("single.txt"), "a\n").unwrap();
    std::fs::write(dir.join("batch.txt"), "a\n").unwrap();
    let mut tools = ToolRegistry::new();
    tools.register(std::sync::Arc::new(EditFileTool::new(ws)));
    let calls = vec![
        call(
            "s1",
            "edit_file",
            json!({"path": "single.txt", "old": "a", "new": "b"}),
        ),
        call(
            "s2",
            "edit_file",
            json!({"path": "single.txt", "old": "b", "new": "c"}),
        ),
        call(
            "b1",
            "edit_file",
            json!({"edits": [{"path": "batch.txt", "old": "a", "new": "b"}]}),
        ),
        call(
            "b2",
            "edit_file",
            json!({"edits": [{"path": "batch.txt", "old": "b", "new": "c"}]}),
        ),
    ];
    let results = Dispatcher::new()
        .execute(
            calls,
            &tools,
            &ExtensionRegistry::new(),
            &CancellationToken::new(),
            None,
            4,
        )
        .await
        .unwrap();
    assert!(results.iter().all(|result| !result.is_error), "{results:?}");
    assert_eq!(
        std::fs::read_to_string(dir.join("single.txt")).unwrap(),
        "c\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("batch.txt")).unwrap(),
        "c\n"
    );
    std::fs::remove_dir_all(dir).ok();
}
