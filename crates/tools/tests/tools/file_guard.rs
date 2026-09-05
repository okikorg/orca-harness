use super::{
    core_tools, ctx, json, temp_ws, EditFileTool, FileGuard, ReadFileTool, Tool, WriteFileTool,
};

/// Read-before-write, the whole contract in one test: a new file needs
/// no read, an existing one does, reading it unblocks the overwrite, and
/// the tool's own write keeps it unblocked.
#[tokio::test]
async fn guarded_write_needs_a_prior_read_of_an_existing_file() {
    let (ws, dir) = temp_ws();
    let guard = FileGuard::new();
    let write = WriteFileTool::new(ws.clone()).guard(guard.clone());
    let read = ReadFileTool::new(ws.clone()).guard(guard.clone());

    // Creating a file is not an overwrite: nothing to have read.
    write
        .call(json!({"path": "new.txt", "content": "one"}), &ctx())
        .await
        .unwrap();
    // ...and having written it, this tool may write it again.
    write
        .call(json!({"path": "new.txt", "content": "two"}), &ctx())
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("new.txt")).unwrap(), "two");

    // A file that appeared from outside must be read first.
    std::fs::write(dir.join("theirs.txt"), "precious").unwrap();
    let err = write
        .call(
            json!({"path": "theirs.txt", "content": "clobbered"}),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("has not been read"), "{err}");
    assert_eq!(
        std::fs::read_to_string(dir.join("theirs.txt")).unwrap(),
        "precious",
        "a refused write must not have happened"
    );

    // Reading it is what makes the overwrite deliberate.
    read.call(json!({"path": "theirs.txt"}), &ctx())
        .await
        .unwrap();
    write
        .call(
            json!({"path": "theirs.txt", "content": "on purpose"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("theirs.txt")).unwrap(),
        "on purpose"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The case the guard exists for: something else edits the file between
/// the read and the write.
#[tokio::test]
async fn guarded_write_refuses_a_file_that_changed_after_the_read() {
    let (ws, dir) = temp_ws();
    let guard = FileGuard::new();
    let write = WriteFileTool::new(ws.clone()).guard(guard.clone());
    let read = ReadFileTool::new(ws.clone()).guard(guard.clone());

    std::fs::write(dir.join("f.txt"), "original").unwrap();
    read.call(json!({"path": "f.txt"}), &ctx()).await.unwrap();

    // Somebody else saves the file (a longer body, so length alone
    // proves the change whatever the filesystem's mtime resolution is).
    std::fs::write(dir.join("f.txt"), "edited by someone else").unwrap();

    let err = write
        .call(
            json!({"path": "f.txt", "content": "from stale context"}),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("changed on disk"), "{err}");
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "edited by someone else"
    );

    // Re-reading adopts the new state and clears the way.
    read.call(json!({"path": "f.txt"}), &ctx()).await.unwrap();
    write
        .call(json!({"path": "f.txt", "content": "now informed"}), &ctx())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "now informed"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `edit_file` is exempt from the check — it works from the contents it
/// just read — but it must still stamp what it wrote, or the next
/// `write_file` to that path would see a file that "changed on disk".
#[tokio::test]
async fn edit_is_exempt_from_the_check_and_still_stamps() {
    let (ws, dir) = temp_ws();
    let guard = FileGuard::new();
    let write = WriteFileTool::new(ws.clone()).guard(guard.clone());
    let edit = EditFileTool::new(ws.clone()).guard(guard.clone());

    std::fs::write(dir.join("f.txt"), "before").unwrap();
    // Never read through the guard, and the edit goes through anyway.
    edit.call(
        json!({"path": "f.txt", "old": "before", "new": "after"}),
        &ctx(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "after");

    // The edit's own write left a stamp, so the overwrite is allowed.
    write
        .call(json!({"path": "f.txt", "content": "replaced"}), &ctx())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "replaced"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Clearing the guard is how a host says "that conversation is over":
/// the next overwrite has to read the file again.
#[tokio::test]
async fn clearing_the_guard_requires_reading_again() {
    let (ws, dir) = temp_ws();
    let guard = FileGuard::new();
    let write = WriteFileTool::new(ws.clone()).guard(guard.clone());

    write
        .call(json!({"path": "f.txt", "content": "one"}), &ctx())
        .await
        .unwrap();
    assert_eq!(guard.len(), 1);
    guard.clear();
    assert!(guard.is_empty());

    let err = write
        .call(json!({"path": "f.txt", "content": "two"}), &ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("has not been read"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Unguarded tools are unchanged: the guard is opt-in, and library users
/// who build the tools themselves get the old behavior.
#[tokio::test]
async fn an_unguarded_write_overwrites_anything() {
    let (ws, dir) = temp_ws();
    let write = WriteFileTool::new(ws.clone());
    std::fs::write(dir.join("f.txt"), "theirs").unwrap();
    write
        .call(json!({"path": "f.txt", "content": "mine"}), &ctx())
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "mine");
    std::fs::remove_dir_all(&dir).ok();
}

/// `core_tools` wires the guard by default, so the shipped set has
/// read-before-write without the host asking for it.
#[tokio::test]
async fn core_tools_ship_with_the_guard_wired() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("f.txt"), "theirs").unwrap();
    let tools = core_tools(&ws);
    let write = tools
        .iter()
        .find(|tool| tool.schema().name == "write_file")
        .expect("write_file in core_tools");
    let err = write
        .call(json!({"path": "f.txt", "content": "mine"}), &ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("has not been read"), "{err}");
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "theirs"
    );
    std::fs::remove_dir_all(&dir).ok();
}
