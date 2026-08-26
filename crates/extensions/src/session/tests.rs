use super::*;
use orca_harness_core::{ToolCall, ToolResult};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("orca-session-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn meta(id: &str) -> SessionMeta {
    SessionMeta {
        v: SESSION_FORMAT_VERSION,
        id: id.into(),
        created_at: 1_755_000_000,
        workspace: "/tmp/ws".into(),
        model: "test-model".into(),
        parent: None,
    }
}

fn write_session(dir: &Path, meta: &SessionMeta, lines: &[String], terminated: bool) -> PathBuf {
    let path = dir.join(format!("{}.jsonl", meta.id));
    let mut body = serde_json::to_string(meta).unwrap() + "\n";
    for (index, line) in lines.iter().enumerate() {
        body.push_str(line);
        if terminated || index + 1 < lines.len() {
            body.push('\n');
        }
    }
    fs::write(&path, body).unwrap();
    path
}

fn msg_lines(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect()
}

#[test]
fn load_round_trips_header_and_messages() {
    let dir = temp_dir("roundtrip");
    let messages = vec![
        Message::System {
            content: "sys".into(),
        },
        Message::User {
            content: "hi".into(),
            images: vec![],
        },
        Message::Assistant {
            content: Some("yo".into()),
            tool_calls: vec![],
        },
    ];
    let path = write_session(&dir, &meta("0000000001-a-0"), &msg_lines(&messages), true);
    let loaded = SessionFile::load(&path).unwrap();
    assert_eq!(loaded.meta, meta("0000000001-a-0"));
    assert!(loaded.warnings.is_empty());
    assert_eq!(
        serde_json::to_string(loaded.context.messages()).unwrap(),
        serde_json::to_string(&messages).unwrap(),
    );
}

#[test]
fn truncated_final_line_is_dropped_with_warning() {
    let dir = temp_dir("truncated");
    let messages = vec![Message::User {
        content: "hi".into(),
        images: vec![],
    }];
    let mut lines = msg_lines(&messages);
    lines.push(r#"{"User":{"conte"#.into());
    let path = write_session(&dir, &meta("0000000001-a-0"), &lines, false);
    let loaded = SessionFile::load(&path).unwrap();
    assert_eq!(loaded.warnings.len(), 1);
    assert_eq!(loaded.context.messages().len(), 1);
}

#[test]
fn corrupt_middle_line_is_an_error() {
    let dir = temp_dir("corrupt");
    let mut lines = vec!["not json".to_string()];
    lines.extend(msg_lines(&[Message::User {
        content: "hi".into(),
        images: vec![],
    }]));
    let path = write_session(&dir, &meta("0000000001-a-0"), &lines, true);
    match SessionFile::load(&path) {
        Err(SessionError::Corrupt { line, .. }) => assert_eq!(line, 2),
        other => panic!("expected Corrupt, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn unsupported_version_is_an_error() {
    let dir = temp_dir("version");
    let mut m = meta("0000000001-a-0");
    m.v = 99;
    let path = write_session(&dir, &m, &[], true);
    assert!(matches!(
        SessionFile::load(&path),
        Err(SessionError::UnsupportedVersion { found: 99, .. })
    ));
}

#[test]
fn dangling_trailing_assistant_tool_calls_are_dropped() {
    let dir = temp_dir("dangling");
    let messages = vec![
        Message::User {
            content: "hi".into(),
            images: vec![],
        },
        Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({}),
            }],
        },
    ];
    let path = write_session(&dir, &meta("0000000001-a-0"), &msg_lines(&messages), true);
    let loaded = SessionFile::load(&path).unwrap();
    assert_eq!(loaded.warnings.len(), 1);
    assert_eq!(loaded.context.messages().len(), 1);
}

#[test]
fn assistant_tool_calls_with_results_survive_load() {
    let dir = temp_dir("paired");
    let call = ToolCall {
        id: "c1".into(),
        name: "shell".into(),
        arguments: serde_json::json!({}),
    };
    let messages = vec![
        Message::Assistant {
            content: None,
            tool_calls: vec![call.clone()],
        },
        Message::Tool {
            results: vec![ToolResult::ok(&call, serde_json::json!("out"))],
        },
    ];
    let path = write_session(&dir, &meta("0000000001-a-0"), &msg_lines(&messages), true);
    let loaded = SessionFile::load(&path).unwrap();
    assert!(loaded.warnings.is_empty());
    assert_eq!(loaded.context.messages().len(), 2);
}

#[test]
fn list_is_newest_first_and_skips_non_sessions() {
    let dir = temp_dir("list");
    write_session(&dir, &meta("0000000001-a-0"), &[], true);
    write_session(&dir, &meta("0000000002-a-0"), &[], true);
    fs::write(dir.join("junk.txt"), "junk").unwrap();
    fs::write(dir.join("bad.jsonl"), "not a header\n").unwrap();
    let sessions = SessionFile::list(&dir);
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].meta.id, "0000000002-a-0");
    assert_eq!(sessions[1].meta.id, "0000000001-a-0");
}

#[test]
fn workspace_key_is_stable_and_filesystem_safe() {
    let a = workspace_key("/Users/dev/my project");
    assert_eq!(a, workspace_key("/Users/dev/my project"));
    assert_ne!(a, workspace_key("/Users/dev/my-project"));
    assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    assert!(workspace_key("///").starts_with("ws-"));
}

#[test]
fn session_ids_sort_chronologically_and_never_collide() {
    let a = new_session_id();
    let b = new_session_id();
    assert_ne!(a, b);
    assert!(a < b || a.split('-').next() == b.split('-').next());
}

#[test]
fn sync_appends_only_new_messages() {
    let dir = temp_dir("sync");
    let handler = SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap();
    let mut context = Context::new();
    context.push_system("sys");
    context.push_user("hi");
    handler.sync(&context);
    context.push_assistant_text("yo");
    handler.sync(&context);
    handler.sync(&context); // no-op

    let loaded = SessionFile::load(&handler.path()).unwrap();
    assert_eq!(
        serde_json::to_string(loaded.context.messages()).unwrap(),
        serde_json::to_string(context.messages()).unwrap(),
    );
}

#[test]
fn sync_rewrites_after_context_shrinks() {
    let dir = temp_dir("shrink");
    let handler = SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap();
    let mut context = Context::new();
    context.push_system("sys");
    context.push_user("one");
    context.push_user("two");
    handler.sync(&context);

    // Compaction replaced the transcript with a shorter one.
    let mut compacted = Context::new();
    compacted.push_system("sys");
    compacted.push_user("summary");
    handler.sync(&compacted);
    compacted.push_user("after");
    handler.sync(&compacted);

    let loaded = SessionFile::load(&handler.path()).unwrap();
    assert_eq!(
        serde_json::to_string(loaded.context.messages()).unwrap(),
        serde_json::to_string(compacted.messages()).unwrap(),
    );
}

#[test]
fn resume_continues_appending_to_the_same_file() {
    let dir = temp_dir("resume");
    let handler = SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap();
    let path = handler.path();
    let mut context = Context::new();
    context.push_system("sys");
    handler.sync(&context);
    drop(handler);

    let (handler, loaded) = SessionHandler::resume(&path).unwrap();
    assert_eq!(loaded.context.messages().len(), 1);
    let mut context = loaded.context;
    context.push_user("again");
    handler.sync(&context);

    let reloaded = SessionFile::load(&path).unwrap();
    assert_eq!(reloaded.context.messages().len(), 2);
}

#[test]
fn start_new_with_context_preserves_old_file_and_records_initial_context() {
    let dir = temp_dir("rotate-with-context");
    let handler = SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap();
    let first = handler.path();
    let mut old = Context::new();
    old.push_system("old sys");
    old.push_user("old turn");
    handler.sync(&old);
    let first_bytes = fs::read(&first).unwrap();
    let mut fresh = Context::new();
    fresh.push_system("fresh sys");

    let new_id = handler.start_new_with_context(&fresh).unwrap();

    assert_eq!(handler.session_id(), new_id);
    assert_ne!(handler.path(), first);
    assert_eq!(fs::read(&first).unwrap(), first_bytes);
    let loaded = SessionFile::load(&handler.path()).unwrap();
    assert_eq!(
        serde_json::to_string(loaded.context.messages()).unwrap(),
        serde_json::to_string(fresh.messages()).unwrap()
    );
}

#[test]
fn start_new_with_context_creation_failure_keeps_old_recorder_active() {
    let dir = temp_dir("rotate-with-context-failure");
    let handler = SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap();
    let old_path = handler.path();
    let old_id = handler.session_id();
    let mut old = Context::new();
    old.push_system("old sys");
    handler.sync(&old);
    let moved = dir.with_extension("preserved");
    fs::rename(&dir, &moved).unwrap();
    fs::write(&dir, "blocks create_dir_all").unwrap();
    let mut fresh = Context::new();
    fresh.push_system("fresh sys");

    assert!(handler.start_new_with_context(&fresh).is_err());
    assert_eq!(handler.session_id(), old_id);
    assert_eq!(handler.path(), old_path);

    fs::remove_file(&dir).unwrap();
    fs::remove_dir_all(moved).unwrap();
}

#[test]
fn start_new_rotates_to_a_fresh_file() {
    let dir = temp_dir("rotate");
    let handler = SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap();
    let first = handler.path();
    let mut context = Context::new();
    context.push_system("sys");
    handler.sync(&context);

    let new_id = handler.start_new().unwrap();
    assert_ne!(handler.path(), first);
    assert_eq!(handler.session_id(), new_id);
    let mut fresh = Context::new();
    fresh.push_system("sys2");
    handler.sync(&fresh);

    assert_eq!(
        SessionFile::load(&first).unwrap().context.messages().len(),
        1
    );
    let second = SessionFile::load(&handler.path()).unwrap();
    assert_eq!(second.context.messages().len(), 1);
    assert_eq!(SessionFile::list(&dir).len(), 2);
}

/// Forking leaves the original file untouched and starts a new one
/// that names it as the parent; the caller's sync fills the branch
/// with whatever context it carried over.
#[test]
fn fork_branches_without_disturbing_the_original() {
    let dir = temp_dir("fork");
    let handler = SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap();
    let original_path = handler.path();
    let original_id = handler.session_id();
    let mut context = Context::new();
    context.push_system("sys");
    context.push_user("one");
    context.push_user("two");
    handler.sync(&context);

    let forked_id = handler.fork().unwrap();
    assert_ne!(forked_id, original_id);
    assert_ne!(handler.path(), original_path);
    handler.sync(&context);

    // The branch holds the whole carried-over context...
    let forked = SessionFile::load(&handler.path()).unwrap();
    assert_eq!(forked.context.messages().len(), 3);
    assert_eq!(forked.meta.parent.as_deref(), Some(original_id.as_str()));
    // ...and the original is exactly as it was left.
    let original = SessionFile::load(&original_path).unwrap();
    assert_eq!(original.context.messages().len(), 3);
    assert_eq!(original.meta.parent, None);
    assert_eq!(SessionFile::list(&dir).len(), 2);

    // Writing on the branch does not touch the original.
    context.push_user("only on the branch");
    handler.sync(&context);
    assert_eq!(
        SessionFile::load(&original_path)
            .unwrap()
            .context
            .messages()
            .len(),
        3
    );
    assert_eq!(
        SessionFile::load(&handler.path())
            .unwrap()
            .context
            .messages()
            .len(),
        4
    );
}

/// `parent` was added after v1 shipped: a header written without it
/// must still load, and one written with it must still be readable
/// as a v1 file.
#[test]
fn headers_without_a_parent_still_load() {
    let dir = temp_dir("parentless");
    let path = dir.join("legacy.jsonl");
    fs::write(
        &path,
        "{\"v\":1,\"id\":\"0000000001-a-0\",\"created_at\":1,\
             \"workspace\":\"/tmp/ws\",\"model\":\"m\"}\n",
    )
    .unwrap();
    let loaded = SessionFile::load(&path).unwrap();
    assert_eq!(loaded.meta.parent, None);

    // A forked header omits nothing else and stays v1.
    let mut forked = meta("0000000002-a-0");
    forked.parent = Some("0000000001-a-0".into());
    let text = serde_json::to_string(&forked).unwrap();
    assert!(text.contains("\"parent\":\"0000000001-a-0\""));
    assert!(text.contains("\"v\":1"));
    // A header with no parent does not write the field at all.
    assert!(!serde_json::to_string(&meta("x"))
        .unwrap()
        .contains("parent"));
}

#[test]
fn reset_empties_the_session_in_place() {
    let dir = temp_dir("reset");
    let handler = SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap();
    let id = handler.session_id();
    let path = handler.path();
    let mut context = Context::new();
    context.push_system("sys");
    context.push_user("hi");
    handler.sync(&context);

    handler.reset().unwrap();
    assert_eq!(handler.session_id(), id, "same session id");
    assert_eq!(handler.path(), path, "same file");
    let loaded = SessionFile::load(&path).unwrap();
    assert_eq!(loaded.context.messages().len(), 0);

    // Recording restarts from zero on the same file.
    let mut fresh = Context::new();
    fresh.push_system("sys2");
    handler.sync(&fresh);
    assert_eq!(
        SessionFile::load(&path).unwrap().context.messages().len(),
        1
    );
    assert_eq!(SessionFile::list(&dir).len(), 1, "no new file");
}

#[test]
fn switch_to_adopts_another_session() {
    let dir = temp_dir("switch");
    let first = SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap();
    let mut context = Context::new();
    context.push_system("old sys");
    context.push_user("old");
    first.sync(&context);
    let old_path = first.path();
    drop(first);

    let handler = SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap();
    let loaded = handler.switch_to(&old_path).unwrap();
    assert_eq!(loaded.context.messages().len(), 2);
    assert_eq!(handler.path(), old_path);
    let mut context = loaded.context;
    context.push_user("new");
    handler.sync(&context);
    assert_eq!(
        SessionFile::load(&old_path)
            .unwrap()
            .context
            .messages()
            .len(),
        3
    );
}

#[test]
fn write_error_disables_recording_and_warns_once() {
    use std::sync::atomic::AtomicUsize;
    let dir = temp_dir("disable");
    let warned = std::sync::Arc::new(AtomicUsize::new(0));
    let count = warned.clone();
    let handler = SessionHandler::create(&dir, "/tmp/ws", "test-model")
        .unwrap()
        .on_warn(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        });
    // Swap the live handle for a read-only one so appends fail.
    handler.inner.lock().unwrap().file = File::open(handler.path()).unwrap();

    let mut context = Context::new();
    context.push_system("sys");
    handler.sync(&context);
    context.push_user("more");
    handler.sync(&context);

    assert_eq!(warned.load(Ordering::SeqCst), 1);
    let loaded = SessionFile::load(&handler.path()).unwrap();
    assert_eq!(loaded.context.messages().len(), 0);
}
