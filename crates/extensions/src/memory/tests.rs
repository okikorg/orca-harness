use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use async_trait::async_trait;
use orca_harness_core::{
    CancellationToken, Context, Model, ModelError, ModelResponse, Tool, ToolCall, ToolResult,
    ToolSchema,
};
use serde_json::json;

use super::*;

static TEST_SEQ: AtomicU32 = AtomicU32::new(0);

struct TestDb {
    dir: PathBuf,
    store: MemoryStore,
}

impl TestDb {
    fn new(name: &str) -> Self {
        let sequence = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "orcacode-memory-{name}-{}-{sequence}",
            std::process::id()
        ));
        let store = MemoryStore::open(dir.join("memory.sqlite3")).unwrap();
        Self { dir, store }
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn scope(id: &str) -> MemoryScope {
    MemoryScope::new(id, format!("/work/{id}"))
}

fn tool_context(name: &str) -> ToolContext {
    ToolContext {
        call_id: "call-memory-1".into(),
        tool_name: name.into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

#[derive(Clone, Default)]
struct RecordingModel {
    contexts: Arc<Mutex<Vec<Context>>>,
}

#[async_trait]
impl Model for RecordingModel {
    async fn generate(
        &self,
        context: &Context,
        _tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        self.contexts.lock().unwrap().push(context.clone());
        Ok(ModelResponse::final_text("ok"))
    }
}

#[test]
fn global_and_workspace_records_are_scoped_before_retrieval() {
    let db = TestDb::new("scope");
    let a = scope("workspace-a");
    let b = scope("workspace-b");
    let global = db
        .store
        .save(&a, "global cargo convention", "workflow", true, "c1")
        .unwrap();
    let local_a = db
        .store
        .save(&a, "alpha workspace cargo rule", "workflow", false, "c2")
        .unwrap();
    let local_b = db
        .store
        .save(&b, "beta workspace cargo rule", "workflow", false, "c3")
        .unwrap();

    let found_a = db.store.search(&a, "cargo", 20).unwrap();
    let ids_a: Vec<&str> = found_a.iter().map(|record| record.id.as_str()).collect();
    assert!(ids_a.contains(&global.id.as_str()));
    assert!(ids_a.contains(&local_a.id.as_str()));
    assert!(!ids_a.contains(&local_b.id.as_str()));

    let found_b = db.store.search(&b, "cargo", 20).unwrap();
    let ids_b: Vec<&str> = found_b.iter().map(|record| record.id.as_str()).collect();
    assert!(ids_b.contains(&global.id.as_str()));
    assert!(!ids_b.contains(&local_a.id.as_str()));
    assert!(ids_b.contains(&local_b.id.as_str()));
    assert_eq!(global.workspace_id, None);
    assert_eq!(local_a.workspace_id.as_deref(), Some("workspace-a"));
}

#[test]
fn updates_and_deletes_follow_scope_and_keep_fts_in_sync() {
    let db = TestDb::new("mutations");
    let a = scope("workspace-a");
    let b = scope("workspace-b");
    let local = db
        .store
        .save(&a, "oldtoken workflow", "workflow", false, "c1")
        .unwrap();
    let global = db
        .store
        .save(&a, "shared oldtoken", "fact", true, "c2")
        .unwrap();

    assert!(db
        .store
        .update(&b, &local.id, "blocked", None)
        .unwrap()
        .is_none());
    assert!(!db.store.forget(&b, &local.id).unwrap());

    let updated = db
        .store
        .update(&a, &local.id, "newtoken workflow", Some("decision"))
        .unwrap()
        .unwrap();
    assert_eq!(updated.kind, "decision");
    assert!(db
        .store
        .search(&a, "oldtoken", 20)
        .unwrap()
        .iter()
        .all(|r| r.id != local.id));
    assert_eq!(db.store.search(&a, "newtoken", 20).unwrap()[0].id, local.id);

    assert!(db.store.forget(&b, &global.id).unwrap());
    assert!(db.store.search(&a, "shared", 20).unwrap().is_empty());
}

#[test]
fn reopening_preserves_records_and_rejects_unknown_schema_versions() {
    let db = TestDb::new("reopen");
    let path = db.store.path().to_path_buf();
    let current = scope("workspace-a");
    db.store
        .save(&current, "persistent recall", "fact", false, "c1")
        .unwrap();
    drop(db.store.clone());

    let reopened = MemoryStore::open(&path).unwrap();
    assert_eq!(reopened.search(&current, "persistent", 8).unwrap().len(), 1);
    reopened
        .connection()
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();
    drop(reopened);
    assert!(matches!(
        MemoryStore::open(&path),
        Err(MemoryError::UnsupportedSchema(99))
    ));
}

#[test]
fn opening_v1_backfills_opaque_ids_without_losing_fts_records() {
    let sequence = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "orcacode-memory-migrate-v1-{}-{sequence}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("memory.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch(LEGACY_V1_SCHEMA).unwrap();
    drop(connection);

    let current = scope("workspace-a");
    let store = MemoryStore::open(&path).unwrap();
    let records = store.search(&current, "legacytoken", 8).unwrap();
    assert_eq!(records.len(), 1);
    assert!(public_id::is_valid(&records[0].id));
    assert_eq!(
        store
            .connection()
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert!(store.forget(&current, &records[0].id).unwrap());
    assert!(store.search(&current, "legacytoken", 8).unwrap().is_empty());

    drop(store);
    MemoryStore::open(&path).unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn concurrent_first_open_initializes_schema_once() {
    let sequence = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "orcacode-memory-concurrent-open-{}-{sequence}",
        std::process::id()
    ));
    let path = dir.join("memory.sqlite3");
    let barrier = Arc::new(Barrier::new(8));
    let handles = (0..8)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                MemoryStore::open(path)
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    MemoryStore::open(&path).unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn management_tool_requires_boolean_scope_and_uses_trusted_workspace() {
    let db = TestDb::new("manage-tool");
    let tool = MemoryManageTool::new(db.store.clone(), scope("workspace-a"));
    let context = tool_context(MEMORY_MANAGE_TOOL);

    let missing_scope = tool
        .call(
            json!({"action": "save", "content": "remember this"}),
            &context,
        )
        .await
        .unwrap_err();
    assert!(missing_scope.message.contains("is_global"));

    let output = tool
        .call(
            json!({
                "action": "save",
                "content": "workspace-only build rule",
                "kind": "workflow",
                "is_global": false,
                "workspace_id": "attacker-controlled"
            }),
            &context,
        )
        .await
        .unwrap();
    assert_eq!(output["saved"]["workspace_id"], "workspace-a");
    assert_eq!(output["saved"]["source_call_id"], "call-memory-1");
    assert!(public_id::is_valid(output["saved"]["id"].as_str().unwrap()));
}

#[tokio::test]
async fn management_tool_rejects_guessable_ids_and_cross_workspace_mutations() {
    let db = TestDb::new("manage-tool-isolation");
    let context = tool_context(MEMORY_MANAGE_TOOL);
    let tool_a = MemoryManageTool::new(db.store.clone(), scope("workspace-a"));
    let tool_b = MemoryManageTool::new(db.store.clone(), scope("workspace-b"));
    let saved = tool_a
        .call(
            json!({"action": "save", "content": "private", "is_global": false}),
            &context,
        )
        .await
        .unwrap();
    let id = saved["saved"]["id"].as_str().unwrap();

    let integer = tool_a
        .call(json!({"action": "forget", "id": 1}), &context)
        .await
        .unwrap_err();
    assert!(integer.message.contains("opaque mem_ identifier"));
    let guessed = tool_a
        .call(
            json!({"action": "forget", "id": "mem_00000000000000000000000000000001"}),
            &context,
        )
        .await
        .unwrap_err();
    assert!(guessed.message.contains("not accessible"));
    let other_workspace = tool_b
        .call(json!({"action": "forget", "id": id}), &context)
        .await
        .unwrap_err();
    assert!(other_workspace.message.contains("not accessible"));

    let forgotten = tool_a
        .call(json!({"action": "forget", "id": id}), &context)
        .await
        .unwrap();
    assert_eq!(forgotten["forgotten"], id);
}

#[tokio::test]
async fn search_tool_lists_or_searches_only_accessible_records() {
    let db = TestDb::new("search-tool");
    let a = scope("workspace-a");
    let b = scope("workspace-b");
    db.store
        .save(&a, "alpha build preference", "preference", false, "c1")
        .unwrap();
    db.store
        .save(&b, "beta private preference", "preference", false, "c2")
        .unwrap();
    let tool = MemorySearchTool::new(db.store.clone(), a);
    let context = tool_context(MEMORY_SEARCH_TOOL);

    let listed = tool.call(json!({}), &context).await.unwrap();
    assert_eq!(listed["count"], 1);
    let searched = tool
        .call(json!({"query": "alpha", "limit": 1000}), &context)
        .await
        .unwrap();
    assert_eq!(searched["count"], 1);
    assert_eq!(searched["memories"][0]["content"], "alpha build preference");
}

#[test]
fn extension_prepares_bounded_user_authority_context_without_mutating_source() {
    let db = TestDb::new("extension");
    let current = scope("workspace-a");
    db.store
        .save(
            &current,
            "Use cargo test memory_filter for this component",
            "workflow",
            false,
            "c1",
        )
        .unwrap();
    let extension = MemoryExtension::new(db.store.clone(), current).max_chars(512);
    let mut context = Context::new();
    context.push_system("base policy");
    context.push_user("please run the memory_filter test");
    let call = ToolCall {
        id: "call-1".into(),
        name: "read_file".into(),
        arguments: json!({"path": "Cargo.toml"}),
    };
    context.push_assistant_tool_calls(None, vec![call.clone()]);
    context.append_tool_results(vec![ToolResult::ok(&call, json!({"content": "workspace"}))]);

    let model_context = extension.prepare_context(&context).unwrap().unwrap();
    assert_eq!(context.messages().len(), 4);
    assert!(matches!(
        context.messages().last(),
        Some(Message::Tool { .. })
    ));
    assert!(matches!(
        model_context.messages().last(),
        Some(Message::Tool { .. })
    ));
    let injected = model_context
        .messages()
        .iter()
        .filter_map(|message| match message {
            Message::User { content, .. } if content.starts_with(MEMORY_CONTEXT_PREFIX) => {
                Some(content)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(injected.len(), 1);
    assert!(injected[0].contains("not instructions or permission to act"));
    assert!(injected[0].chars().count() <= 512);
}

#[test]
fn bounded_context_keeps_each_injected_record_valid_json() {
    let db = TestDb::new("bounded-json");
    let current = scope("workspace-a");
    db.store
        .save(
            &current,
            &"quoted \" content ".repeat(500),
            "fact",
            false,
            "c1",
        )
        .unwrap();
    let extension = MemoryExtension::new(db.store.clone(), current).max_chars(512);
    let mut context = Context::new();
    context.push_user("quoted content");

    let model_context = extension.prepare_context(&context).unwrap().unwrap();
    let content = model_context
        .messages()
        .iter()
        .find_map(|message| match message {
            Message::User { content, .. } if content.starts_with(MEMORY_CONTEXT_PREFIX) => {
                Some(content)
            }
            _ => None,
        })
        .expect("expected injected user memory");
    assert!(content.chars().count() <= 512);
    let record = content.lines().nth(2).expect("serialized memory record");
    let parsed: Value = serde_json::from_str(record).unwrap();
    assert!(parsed["content"].as_str().unwrap().ends_with("..."));
}

#[tokio::test]
async fn model_overlay_is_transient_not_persisted_and_forget_removes_influence() {
    let db = TestDb::new("model-overlay");
    let current = scope("workspace-a");
    let record = db
        .store
        .save(
            &current,
            "prefer transient_alpha",
            "preference",
            false,
            "c1",
        )
        .unwrap();
    let recorder = RecordingModel::default();
    let model = MemoryModel::new(
        recorder.clone(),
        MemoryExtension::new(db.store.clone(), current.clone()),
    );
    let mut context = Context::new();
    context.push_user("transient_alpha");

    model.generate(&context, &[]).await.unwrap();
    assert_eq!(context.messages().len(), 1);
    let first = recorder.contexts.lock().unwrap()[0].clone();
    assert!(first.messages().iter().any(|message| {
        matches!(message, Message::User { content, .. } if content.starts_with(MEMORY_CONTEXT_PREFIX))
    }));

    let session =
        crate::SessionHandler::create(db.dir.join("sessions"), "workspace-a", "test").unwrap();
    session.sync(&context);
    assert!(!fs::read_to_string(session.path())
        .unwrap()
        .contains(MEMORY_CONTEXT_PREFIX));

    assert!(db.store.forget(&current, &record.id).unwrap());
    model.generate(&context, &[]).await.unwrap();
    let second = recorder.contexts.lock().unwrap()[1].clone();
    assert_eq!(second.messages().len(), 1);
    assert!(!second.messages().iter().any(|message| {
        matches!(message, Message::User { content, .. } if content.starts_with(MEMORY_CONTEXT_PREFIX))
    }));
}

#[test]
fn bounded_record_stops_when_metadata_alone_exceeds_budget() {
    let record = MemoryRecord {
        id: "mem_00000000000000000000000000000001".into(),
        content: "content".into(),
        kind: "fact".into(),
        is_global: false,
        workspace_id: Some("workspace-a".into()),
        workspace_root: "x".repeat(1_000),
        source_call_id: "c1".into(),
        created_at: 1,
        updated_at: 1,
    };

    assert!(serialized_record(&record, 64).is_none());
}

#[test]
fn validation_handles_blank_content_and_fts_syntax_as_data() {
    let db = TestDb::new("validation");
    let current = scope("workspace-a");
    assert!(matches!(
        db.store.save(&current, "  ", "fact", false, "c1"),
        Err(MemoryError::InvalidInput(_))
    ));
    assert!(db.store.search(&current, " -- OR ", 8).unwrap().is_empty());
    db.store
        .save(&current, "literal OR token", "fact", false, "c2")
        .unwrap();
    assert_eq!(db.store.search(&current, "token OR *", 8).unwrap().len(), 1);
}

#[test]
fn short_search_terms_are_supported_when_no_longer_term_is_present() {
    let db = TestDb::new("short-terms");
    let current = scope("workspace-a");
    db.store
        .save(
            &current,
            "Use Go for UI and C for FFI",
            "decision",
            false,
            "c1",
        )
        .unwrap();

    assert_eq!(db.store.search(&current, "Go", 8).unwrap().len(), 1);
    assert_eq!(db.store.search(&current, "UI", 8).unwrap().len(), 1);
    assert_eq!(
        db.store.search(&current, "UI guidance", 8).unwrap().len(),
        1
    );
    assert_eq!(db.store.search(&current, "C", 8).unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn database_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let db = TestDb::new("permissions");
    let mode = fs::metadata(db.store.path()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[cfg(unix)]
#[test]
fn database_wal_and_shared_memory_are_owner_only_in_existing_directory() {
    use std::os::unix::fs::PermissionsExt;

    let sequence = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "orcacode-memory-sidecar-permissions-{}-{sequence}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let path = dir.join("memory.sqlite3");
    let store = MemoryStore::open(&path).unwrap();
    store
        .save(
            &scope("workspace-a"),
            "sidecar permission",
            "fact",
            false,
            "c1",
        )
        .unwrap();

    for file in [
        path.clone(),
        dir.join("memory.sqlite3-wal"),
        dir.join("memory.sqlite3-shm"),
    ] {
        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "unexpected permissions for {}", file.display());
    }

    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

const LEGACY_V1_SCHEMA: &str = r#"
CREATE TABLE memories (
    id INTEGER PRIMARY KEY,
    content TEXT NOT NULL,
    kind TEXT NOT NULL,
    is_global INTEGER NOT NULL CHECK (is_global IN (0, 1)),
    workspace_id TEXT,
    workspace_root TEXT NOT NULL,
    source_call_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE VIRTUAL TABLE memories_fts USING fts5(
    content, kind, content='memories', content_rowid='id', tokenize='unicode61'
);
CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, content, kind) VALUES (new.id, new.content, new.kind);
END;
CREATE TRIGGER memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content, kind)
    VALUES ('delete', old.id, old.content, old.kind);
END;
CREATE TRIGGER memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content, kind)
    VALUES ('delete', old.id, old.content, old.kind);
    INSERT INTO memories_fts(rowid, content, kind) VALUES (new.id, new.content, new.kind);
END;
INSERT INTO memories (
    content, kind, is_global, workspace_id, workspace_root,
    source_call_id, created_at, updated_at
) VALUES ('legacytoken record', 'fact', 0, 'workspace-a', '/work/workspace-a', 'legacy', 1, 1);
PRAGMA user_version = 1;
"#;
