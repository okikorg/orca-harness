use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use orca_harness_core::testing::call;
use orca_harness_core::{
    Agent, CancellationToken, Concurrency, Dispatcher, Extension, ExtensionRegistry, Next,
    Subscriptions, Tool, ToolCall, ToolContext, ToolError, ToolRegistry,
};
use orca_harness_tools::{ApplyPatchTool, FileGuard, MultiEditTool, MutationPreflight, Workspace};
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
async fn malformed_multi_edit_is_rejected_before_later_review_extensions() {
    let (ws, dir) = temp_ws();
    let reviewed = Arc::new(AtomicBool::new(false));
    let model = orca_harness_core::testing::ScriptedModel::tool_round(
        vec![call(
            "invalid",
            "multi_edit",
            json!({"edits": [{"path": "a.md", "old": "", "new": "notice"}]}),
        )],
        "done",
    );

    let answer = Agent::new(model)
        .tool(MultiEditTool::new(ws))
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
async fn multi_edit_applies_ordered_cross_file_replacements() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("a.txt"), "one one\n").unwrap();
    std::fs::write(dir.join("b.txt"), "alpha\n").unwrap();
    let guard = FileGuard::new();
    let tool = MultiEditTool::new(ws).guard(guard.clone());

    let output = tool
        .call(
            json!({"edits": [
                {"path": "a.txt", "old": "one", "new": "two", "replaceAll": true},
                {"path": "a.txt", "old": "two two", "new": "three"},
                {"path": "b.txt", "old": "alpha", "new": "beta"}
            ]}),
            &ctx("multi_edit"),
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
async fn multi_edit_appends_to_multiple_files_atomically() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("a.md"), "# A\n").unwrap();
    std::fs::write(dir.join("b.md"), "# B\n").unwrap();
    let tool = MultiEditTool::new(ws);

    let output = tool
        .call(
            json!({"edits": [
                {"path": "a.md", "operation": "append", "content": "\nnotice a\n"},
                {"path": "b.md", "operation": "append", "content": "\nnotice b\n"}
            ]}),
            &ctx("multi_edit"),
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
async fn multi_edit_append_preflight_failure_writes_nothing() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("a.md"), "stable\n").unwrap();
    let tool = MultiEditTool::new(ws);

    let error = tool
        .call(
            json!({"edits": [
                {"path": "a.md", "operation": "append", "content": "changed\n"},
                {"path": "missing.md", "operation": "append", "content": "new\n"}
            ]}),
            &ctx("multi_edit"),
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
async fn multi_edit_empty_replacement_points_to_append_operation() {
    let (ws, dir) = temp_ws();
    let tool = MultiEditTool::new(ws);
    let error = tool
        .call(
            json!({"edits": [{"path": "a.md", "old": "", "new": "notice"}]}),
            &ctx("multi_edit"),
        )
        .await
        .unwrap_err();

    assert!(error.message.contains("operation: \"append\""));
    assert!(error.message.contains("apply_patch"));
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn multi_edit_preflight_failure_writes_nothing() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("a.txt"), "before\n").unwrap();
    std::fs::write(dir.join("b.txt"), "stable\n").unwrap();
    let tool = MultiEditTool::new(ws);

    let error = tool
        .call(
            json!({"edits": [
                {"path": "a.txt", "old": "before", "new": "after"},
                {"path": "b.txt", "old": "missing", "new": "changed"}
            ]}),
            &ctx("multi_edit"),
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
fn multi_edit_locks_every_distinct_path() {
    let (ws, dir) = temp_ws();
    let tool = MultiEditTool::new(ws);
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
    let multi = MultiEditTool::new(ws.clone());
    assert_eq!(
        multi.concurrency(&json!({"edits": [
            {"path": "./nested/../a.txt", "old": "a", "new": "A"},
            {"path": "a.txt", "old": "A", "new": "B"}
        ]})),
        Concurrency::Keys(vec!["file:a.txt".into()])
    );

    let patch = ApplyPatchTool::new(ws);
    assert_eq!(
        patch.concurrency(&json!({"patch": "*** Begin Patch\n*** Update File: ./nested/../a.txt\n@@\n-a\n+b\n*** End Patch"})),
        Concurrency::Keys(vec!["file:a.txt".into()])
    );
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn aliases_have_safe_same_call_semantics() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();

    let multi = MultiEditTool::new(ws.clone());
    multi
        .call(
            json!({"edits": [
                {"path": "./nested/../a.txt", "old": "a", "new": "b"},
                {"path": "a.txt", "old": "b", "new": "c"}
            ]}),
            &ctx("multi_edit"),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "c\n");

    let patch = ApplyPatchTool::new(ws);
    let error = patch
        .call(
            json!({"patch": "*** Begin Patch\n*** Update File: a.txt\n@@\n-c\n+d\n*** Update File: ./nested/../a.txt\n@@\n-c\n+e\n*** End Patch"}),
            &ctx("apply_patch"),
        )
        .await
        .unwrap_err();
    assert!(error.message.contains("resolve to the same path"));
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "c\n");
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn apply_patch_updates_adds_and_deletes_after_full_preflight() {
    let (ws, dir) = temp_ws();
    std::fs::write(
        dir.join("a.txt"),
        "alpha\nold one\nmiddle\nold two\nomega\n",
    )
    .unwrap();
    std::fs::write(dir.join("gone.txt"), "remove me\n").unwrap();
    let guard = FileGuard::new();
    let tool = ApplyPatchTool::new(ws).guard(guard.clone());
    let patch = "*** Begin Patch
*** Update File: a.txt
@@
 alpha
-old one
+new one
@@
 middle
-old two
+new two
 omega
*** Add File: added.txt
+new file
+second line
*** Delete File: gone.txt
*** End Patch";

    let output = tool
        .call(json!({"patch": patch}), &ctx("apply_patch"))
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "alpha\nnew one\nmiddle\nnew two\nomega\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("added.txt")).unwrap(),
        "new file\nsecond line\n"
    );
    assert!(!dir.join("gone.txt").exists());
    assert_eq!(
        output,
        json!({
            "filesChanged": 3,
            "added": 1,
            "updated": 1,
            "deleted": 1,
            "paths": ["a.txt", "added.txt", "gone.txt"]
        })
    );
    assert_eq!(guard.len(), 2);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn apply_patch_preserves_crlf_and_rejects_ambiguous_hunks() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("crlf.txt"), "first\r\nold\r\nlast\r\n").unwrap();
    std::fs::write(dir.join("ambiguous.txt"), "same\nother\nsame\n").unwrap();
    let tool = ApplyPatchTool::new(ws);
    tool.call(
        json!({"patch": "*** Begin Patch\n*** Update File: crlf.txt\n@@\n-old\n+new\n*** End Patch"}),
        &ctx("apply_patch"),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(dir.join("crlf.txt")).unwrap(),
        b"first\r\nnew\r\nlast\r\n"
    );

    let error = tool
        .call(
            json!({"patch": "*** Begin Patch\n*** Update File: ambiguous.txt\n@@\n-same\n+changed\n*** End Patch"}),
            &ctx("apply_patch"),
        )
        .await
        .unwrap_err();
    assert!(error.message.contains("matched 2 locations"));
    assert_eq!(
        std::fs::read_to_string(dir.join("ambiguous.txt")).unwrap(),
        "same\nother\nsame\n"
    );
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn apply_patch_preflight_failure_does_not_create_earlier_file() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("source.txt"), "current\n").unwrap();
    let tool = ApplyPatchTool::new(ws);
    let error = tool
        .call(
            json!({"patch": "*** Begin Patch\n*** Add File: early.txt\n+created\n*** Update File: source.txt\n@@\n-missing\n+changed\n*** End Patch"}),
            &ctx("apply_patch"),
        )
        .await
        .unwrap_err();
    assert!(error.message.contains("did not match"));
    assert!(!dir.join("early.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("source.txt")).unwrap(),
        "current\n"
    );
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn no_op_edits_do_not_write_or_restamp_files() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("same.txt"), "same\n").unwrap();
    let guard = FileGuard::new();

    let multi = MultiEditTool::new(ws.clone()).guard(guard.clone());
    let output = multi
        .call(
            json!({"edits": [{"path": "same.txt", "old": "same", "new": "same"}]}),
            &ctx("multi_edit"),
        )
        .await
        .unwrap();
    assert_eq!(output["filesChanged"], 0);
    assert_eq!(output["replacements"], 1);
    assert_eq!(guard.len(), 0);

    let patch = ApplyPatchTool::new(ws).guard(guard.clone());
    let output = patch
        .call(
            json!({"patch": "*** Begin Patch\n*** Update File: same.txt\n@@\n-same\n+same\n*** End Patch"}),
            &ctx("apply_patch"),
        )
        .await
        .unwrap();
    assert_eq!(output["filesChanged"], 0);
    assert_eq!(guard.len(), 0);
    assert_eq!(
        std::fs::read_to_string(dir.join("same.txt")).unwrap(),
        "same\n"
    );
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn apply_patch_rejects_hunk_larger_than_file_without_panicking() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("short.txt"), "one\n").unwrap();
    let tool = ApplyPatchTool::new(ws);
    let error = tool
        .call(
            json!({"patch": "*** Begin Patch\n*** Update File: short.txt\n@@\n one\n-two\n-three\n+changed\n*** End Patch"}),
            &ctx("apply_patch"),
        )
        .await
        .unwrap_err();
    assert!(error.message.contains("did not match"));
    assert_eq!(
        std::fs::read_to_string(dir.join("short.txt")).unwrap(),
        "one\n"
    );
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn apply_patch_locks_every_distinct_path_and_rejects_escape() {
    let (ws, dir) = temp_ws();
    let tool = ApplyPatchTool::new(ws);
    let patch = "*** Begin Patch\n*** Delete File: b.txt\n*** Add File: a.txt\n+x\n*** End Patch";
    assert_eq!(
        tool.concurrency(&json!({"patch": patch})),
        Concurrency::Keys(vec!["file:a.txt".into(), "file:b.txt".into()])
    );
    std::fs::write(dir.join("inside.txt"), "safe\n").unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let error = runtime
        .block_on(tool.call(
            json!({"patch": "*** Begin Patch\n*** Delete File: ../outside.txt\n*** End Patch"}),
            &ctx("apply_patch"),
        ))
        .unwrap_err();
    assert!(error.message.contains("escapes the workspace root"));
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_path_calls_serialize_in_model_order() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("multi.txt"), "a\n").unwrap();
    std::fs::write(dir.join("patch.txt"), "a\n").unwrap();
    let mut tools = ToolRegistry::new();
    tools.register(std::sync::Arc::new(MultiEditTool::new(ws.clone())));
    tools.register(std::sync::Arc::new(ApplyPatchTool::new(ws)));
    let calls = vec![
        call(
            "m1",
            "multi_edit",
            json!({"edits": [{"path": "multi.txt", "old": "a", "new": "b"}]}),
        ),
        call(
            "m2",
            "multi_edit",
            json!({"edits": [{"path": "multi.txt", "old": "b", "new": "c"}]}),
        ),
        call(
            "p1",
            "apply_patch",
            json!({"patch": "*** Begin Patch\n*** Update File: patch.txt\n@@\n-a\n+b\n*** End Patch"}),
        ),
        call(
            "p2",
            "apply_patch",
            json!({"patch": "*** Begin Patch\n*** Update File: patch.txt\n@@\n-b\n+c\n*** End Patch"}),
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
        std::fs::read_to_string(dir.join("multi.txt")).unwrap(),
        "c\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("patch.txt")).unwrap(),
        "c\n"
    );
    std::fs::remove_dir_all(dir).ok();
}
