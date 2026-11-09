//! `Workspace::with_added_dirs`: every file tool reaches the main root
//! and each added directory, and nothing else. Each tool is checked four
//! ways: inside the main root, inside an added directory, outside every
//! root, and through a symlink that leads out.

#![cfg(unix)]

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{CancellationToken, Tool, ToolContext};
use orca_harness_tools::{
    CopyFileTool, EditFileTool, FileGuard, GlobTool, GrepTool, ReadFileTool, SubagentTool,
    Workspace, WriteFileTool,
};
use serde_json::{json, Value};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// `root/` (the workspace), `added/` (an added directory) and `outside/`
/// (neither), each holding `note.txt`; `root/leak` and `added/leak` are
/// symlinks to `outside/`.
struct Layout {
    base: PathBuf,
    root: PathBuf,
    added: PathBuf,
    outside: PathBuf,
    ws: Workspace,
}

impl Layout {
    fn new() -> Self {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let base = std::env::temp_dir().join(format!("orca-added-dirs-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let [root, added, outside] = ["root", "added", "outside"].map(|name| {
            let dir = base.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("note.txt"), format!("needle in {name}\n")).unwrap();
            dir.canonicalize().unwrap()
        });
        symlink(&outside, root.join("leak")).unwrap();
        symlink(&outside, added.join("leak")).unwrap();
        let ws = Workspace::new(&root).with_added_dirs([&added]).unwrap();
        Self {
            base,
            root,
            added,
            outside,
            ws,
        }
    }

    fn abs(dir: &Path, rel: &str) -> String {
        dir.join(rel).display().to_string()
    }
}

impl Drop for Layout {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn ctx(name: &str) -> ToolContext {
    ToolContext {
        call_id: "t".into(),
        tool_name: name.into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

async fn run(tool: &dyn Tool, input: Value) -> Result<Value, String> {
    let name = tool.schema().name;
    tool.call(input, &ctx(&name))
        .await
        .map_err(|error| error.to_string())
}

fn refused(result: Result<Value, String>) -> String {
    let error = result.expect_err("expected the path to be refused");
    assert!(
        error.contains("outside the workspace"),
        "unexpected refusal: {error}"
    );
    error
}

#[tokio::test]
async fn read_file_reaches_every_root_and_nothing_else() {
    let l = Layout::new();
    let read = ReadFileTool::new(l.ws.clone());
    let main = run(&read, json!({"path": "note.txt"})).await.unwrap();
    assert!(main.to_string().contains("needle in root"));
    let main_abs = run(&read, json!({"path": Layout::abs(&l.root, "note.txt")})).await;
    assert!(main_abs.unwrap().to_string().contains("needle in root"));
    let added = run(&read, json!({"path": Layout::abs(&l.added, "note.txt")})).await;
    assert!(added.unwrap().to_string().contains("needle in added"));
    refused(run(&read, json!({"path": Layout::abs(&l.outside, "note.txt")})).await);
    refused(
        run(
            &read,
            json!({"path": Layout::abs(&l.added, "leak/note.txt")}),
        )
        .await,
    );
    refused(run(&read, json!({"path": "leak/note.txt"})).await);
    refused(
        run(
            &read,
            json!({"path": Layout::abs(&l.added, "../outside/note.txt")}),
        )
        .await,
    );
}

#[tokio::test]
async fn write_file_creates_inside_roots_only() {
    let l = Layout::new();
    let write = WriteFileTool::new(l.ws.clone()).guard(FileGuard::new());
    run(&write, json!({"path": "new.txt", "content": "a"}))
        .await
        .unwrap();
    assert!(l.root.join("new.txt").exists());
    let target = Layout::abs(&l.added, "sub/new.txt");
    run(&write, json!({"path": target, "content": "b"}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(l.added.join("sub/new.txt")).unwrap(),
        "b"
    );
    refused(
        run(
            &write,
            json!({"path": Layout::abs(&l.outside, "x.txt"), "content": "c"}),
        )
        .await,
    );
    refused(
        run(
            &write,
            json!({"path": Layout::abs(&l.added, "leak/x.txt"), "content": "c"}),
        )
        .await,
    );
    refused(run(&write, json!({"path": "leak/x.txt", "content": "c"})).await);
    assert!(!l.outside.join("x.txt").exists(), "nothing written outside");
}

#[tokio::test]
async fn symlink_refusal_names_the_link_but_never_its_target() {
    let l = Layout::new();
    symlink(l.outside.join("note.txt"), l.root.join("secret.txt")).unwrap();
    let read = ReadFileTool::new(l.ws.clone());
    for (path, link) in [
        ("leak/note.txt".to_string(), l.root.join("leak")),
        (Layout::abs(&l.added, "leak/note.txt"), l.added.join("leak")),
        ("secret.txt".to_string(), l.root.join("secret.txt")),
    ] {
        let error = refused(run(&read, json!({ "path": path })).await);
        assert!(error.contains("through a symlink"), "{error}");
        assert!(
            error.contains(&format!(
                "{} points outside every allowed folder",
                link.display()
            )),
            "{error}"
        );
        assert!(error.contains("the user can allow extra folders with --add-dir"));
        // Where the link leads stays out of the message: a planted link
        // must not become a ready-made folder to add.
        assert!(!error.contains(&l.outside.display().to_string()), "{error}");
    }
    // Plain outside paths keep the plain message.
    let error = refused(run(&read, json!({"path": Layout::abs(&l.outside, "note.txt")})).await);
    assert!(!error.contains("--add-dir"), "{error}");
}

#[tokio::test]
async fn write_file_refuses_a_dangling_symlink_into_nowhere() {
    let l = Layout::new();
    symlink(l.outside.join("created-by-link"), l.added.join("dangling")).unwrap();
    let write = WriteFileTool::new(l.ws.clone()).guard(FileGuard::new());
    refused(
        run(
            &write,
            json!({"path": Layout::abs(&l.added, "dangling"), "content": "x"}),
        )
        .await,
    );
    assert!(!l.outside.join("created-by-link").exists());
}

#[tokio::test]
async fn edit_file_edits_inside_roots_only() {
    let l = Layout::new();
    let edit = EditFileTool::new(l.ws.clone());
    let replace = |path: String, from: &str| json!({"path": path, "old": from, "new": "pin"});
    run(&edit, replace("note.txt".into(), "needle in root"))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(l.root.join("note.txt")).unwrap(),
        "pin\n"
    );
    run(
        &edit,
        replace(Layout::abs(&l.added, "note.txt"), "needle in added"),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(l.added.join("note.txt")).unwrap(),
        "pin\n"
    );
    refused(
        run(
            &edit,
            replace(Layout::abs(&l.outside, "note.txt"), "needle"),
        )
        .await,
    );
    refused(
        run(
            &edit,
            replace(Layout::abs(&l.added, "leak/note.txt"), "needle"),
        )
        .await,
    );
    assert_eq!(
        std::fs::read_to_string(l.outside.join("note.txt")).unwrap(),
        "needle in outside\n"
    );
}

#[tokio::test]
async fn glob_and_grep_search_inside_roots_only() {
    let l = Layout::new();
    let glob = GlobTool::new(l.ws.clone());
    let grep = GrepTool::new(l.ws.clone());
    for (tool, input) in [
        (&glob as &dyn Tool, json!({"pattern": "*.txt"})),
        (&grep as &dyn Tool, json!({"query": "needle"})),
    ] {
        let main = run(tool, input.clone()).await.unwrap().to_string();
        assert!(main.contains("note.txt"), "{main}");
        let mut in_added = input.clone();
        in_added["path"] = json!(l.added.display().to_string());
        let added = run(tool, in_added).await.unwrap().to_string();
        // Results in an added directory come back absolute: the form the
        // model needs to address them again.
        assert!(
            added.contains(&Layout::abs(&l.added, "note.txt")),
            "{added}"
        );
        for path in [
            l.outside.display().to_string(),
            Layout::abs(&l.added, "leak"),
            "leak".to_string(),
        ] {
            let mut escape = input.clone();
            escape["path"] = json!(path);
            refused(run(tool, escape).await);
        }
    }
}

#[tokio::test]
async fn grep_does_not_follow_symlinks_out_of_every_root() {
    let l = Layout::new();
    symlink(l.outside.join("note.txt"), l.root.join("secret.txt")).unwrap();
    symlink(l.outside.join("note.txt"), l.added.join("secret.txt")).unwrap();
    // A link that stays inside the roots is still searched.
    symlink(l.added.join("note.txt"), l.root.join("to-added.txt")).unwrap();
    let grep = GrepTool::new(l.ws.clone());
    for path in [".".to_string(), l.added.display().to_string()] {
        let out = run(&grep, json!({"query": "needle", "path": path}))
            .await
            .unwrap()
            .to_string();
        assert!(!out.contains("needle in outside"), "{out}");
    }
    let out = run(&grep, json!({"query": "needle in added"}))
        .await
        .unwrap()
        .to_string();
    assert!(out.contains("to-added.txt"), "{out}");
}

#[tokio::test]
async fn copy_file_moves_between_roots_but_not_out() {
    let l = Layout::new();
    let copy = CopyFileTool::new(l.ws.clone());
    let into_added = Layout::abs(&l.added, "copied.txt");
    run(&copy, json!({"from": "note.txt", "to": into_added}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(l.added.join("copied.txt")).unwrap(),
        "needle in root\n"
    );
    let out = Layout::abs(&l.outside, "copied.txt");
    refused(run(&copy, json!({"from": "note.txt", "to": out})).await);
    refused(
        run(
            &copy,
            json!({"from": Layout::abs(&l.outside, "note.txt"), "to": "x.txt"}),
        )
        .await,
    );
}

#[tokio::test]
async fn without_added_dirs_absolute_paths_outside_the_root_stay_refused() {
    let l = Layout::new();
    let read = ReadFileTool::new(Workspace::new(&l.root));
    refused(run(&read, json!({"path": Layout::abs(&l.added, "note.txt")})).await);
    let escape = run(&read, json!({"path": "../added/note.txt"})).await;
    assert!(escape.unwrap_err().contains("escapes the workspace root"));
}

#[test]
fn added_dirs_must_exist_and_are_stored_canonical() {
    let l = Layout::new();
    let missing = Workspace::new(&l.root).with_added_dirs([l.base.join("nope")]);
    assert!(missing.unwrap_err().to_string().contains("added directory"));
    let file = Workspace::new(&l.root).with_added_dirs([l.root.join("note.txt")]);
    assert!(file.unwrap_err().to_string().contains("not a directory"));
    // Spelled through a symlink and `..` (`leak/..` is `outside/..`, the
    // base), the stored form is canonical.
    let ws = Workspace::new(&l.root)
        .with_added_dirs([l.root.join("leak/../added")])
        .unwrap();
    assert_eq!(ws.added_dirs(), std::slice::from_ref(&l.added));
}

#[tokio::test]
async fn subagents_inherit_added_directories() {
    let l = Layout::new();
    let target = Layout::abs(&l.added, "from-child.txt");
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call(
            "1",
            "write_file",
            json!({"path": target, "content": "child"}),
        )],
        "done",
    ));
    let tool = SubagentTool::new(model, &l.ws);
    let out = tool
        .call(
            json!({"task": "write into the added dir"}),
            &ctx("subagent"),
        )
        .await
        .unwrap();
    assert_eq!(out["answer"], "done");
    assert_eq!(
        std::fs::read_to_string(l.added.join("from-child.txt")).unwrap(),
        "child"
    );
}

#[tokio::test]
async fn subagents_stay_confined_to_the_parents_roots() {
    let l = Layout::new();
    let target = Layout::abs(&l.outside, "from-child.txt");
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call(
            "1",
            "write_file",
            json!({"path": target, "content": "x"}),
        )],
        "done",
    ));
    SubagentTool::new(model, &l.ws)
        .call(json!({"task": "try to escape"}), &ctx("subagent"))
        .await
        .unwrap();
    assert!(!l.outside.join("from-child.txt").exists());
}
