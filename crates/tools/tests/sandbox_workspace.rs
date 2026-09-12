//! File tools rooted at a sandboxed [`Workspace`].
//!
//! `core_tools_with_executor` has always carried the warning that file
//! tools "still operate on the local workspace" — so a sandboxed shell
//! beside them was a boundary with a hole in it. These tests pin the
//! property that closes it: every file operation lands in the provider,
//! and the host path of the same name is never created, read, or written.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use orca_harness_core::{
    CancellationToken, Capabilities, Entry, ExecOutput, ExecRequest, FileMode, Output, Sandbox,
    SandboxError, Session, SpawnRequest, Stat, Tool, ToolContext,
};
use orca_harness_tools::{FileGuard, Workspace};

/// A sandbox whose whole filesystem is a map, so anything the tools write
/// to the real disk shows up as an absence here and a presence there.
#[derive(Default)]
struct MemSandbox {
    files: Mutex<HashMap<String, Vec<u8>>>,
    execs: Mutex<Vec<String>>,
}

impl MemSandbox {
    fn get(&self, path: &str) -> Option<Vec<u8>> {
        self.files.lock().unwrap().get(path).cloned()
    }

    fn put(&self, path: &str, bytes: &[u8]) {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_string(), bytes.to_vec());
    }
}

#[async_trait]
impl Sandbox for MemSandbox {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            sessions: true,
            file_api: true,
            network_policy: false,
        }
    }

    async fn exec(&self, request: ExecRequest) -> Result<ExecOutput, SandboxError> {
        self.execs.lock().unwrap().push(request.command.clone());
        // Only `rm -f` reaches here from the file tools.
        if let Some(rest) = request.command.strip_prefix("rm -f ") {
            let path = rest.trim().trim_matches('\'');
            self.files.lock().unwrap().remove(path);
        }
        Ok(ExecOutput::default())
    }

    async fn spawn(&self, _: SpawnRequest) -> Result<(Box<dyn Session>, Output), SandboxError> {
        Err(SandboxError::Unsupported {
            provider: "mem",
            capability: "sessions",
        })
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError> {
        self.get(path)
            .ok_or_else(|| SandboxError::Request(format!("no such file: {path}")))
    }

    async fn write_file(&self, path: &str, bytes: &[u8], _: FileMode) -> Result<(), SandboxError> {
        self.put(path, bytes);
        Ok(())
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<Entry>, SandboxError> {
        let prefix = format!("{}/", path.trim_end_matches('/'));
        let mut names: Vec<Entry> = Vec::new();
        for key in self.files.lock().unwrap().keys() {
            let Some(rest) = key.strip_prefix(&prefix) else {
                continue;
            };
            let (name, is_dir) = match rest.split_once('/') {
                Some((head, _)) => (head.to_string(), true),
                None => (rest.to_string(), false),
            };
            if !names.iter().any(|e| e.name == name) {
                names.push(Entry { name, is_dir });
            }
        }
        Ok(names)
    }

    async fn stat(&self, path: &str) -> Result<Option<Stat>, SandboxError> {
        if let Some(bytes) = self.get(path) {
            return Ok(Some(Stat {
                modified: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1)),
                len: bytes.len() as u64,
                is_dir: false,
            }));
        }
        // A directory is anything that prefixes a known file.
        let prefix = format!("{}/", path.trim_end_matches('/'));
        let is_dir = self
            .files
            .lock()
            .unwrap()
            .keys()
            .any(|key| key.starts_with(&prefix));
        Ok(is_dir.then_some(Stat {
            modified: None,
            len: 0,
            is_dir: true,
        }))
    }

    async fn shutdown(&self) -> Result<(), SandboxError> {
        Ok(())
    }
}

fn ctx(name: &str) -> ToolContext {
    ToolContext {
        call_id: "test".into(),
        tool_name: name.into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

fn tools(sandbox: &Arc<MemSandbox>) -> (Workspace, FileGuard) {
    (
        Workspace::sandboxed("/workspace", sandbox.clone()),
        FileGuard::new(),
    )
}

#[tokio::test]
async fn writes_and_reads_land_in_the_sandbox_not_on_the_host() {
    let sandbox = Arc::new(MemSandbox::default());
    let (ws, guard) = tools(&sandbox);
    assert!(ws.is_sandboxed());

    let write = orca_harness_tools::WriteFileTool::new(ws.clone()).guard(guard.clone());
    write
        .call(
            serde_json::json!({ "path": "notes.md", "content": "hello" }),
            &ctx("write_file"),
        )
        .await
        .expect("write");

    // In the sandbox, at the resolved absolute path.
    assert_eq!(
        sandbox.get("/workspace/notes.md").as_deref(),
        Some(&b"hello"[..])
    );
    // And nowhere on this machine.
    assert!(
        !std::path::Path::new("/workspace/notes.md").exists(),
        "the host filesystem must not have been touched"
    );

    let read = orca_harness_tools::ReadFileTool::new(ws).guard(guard);
    let out = read
        .call(serde_json::json!({ "path": "notes.md" }), &ctx("read_file"))
        .await
        .expect("read");
    assert!(out["content"].as_str().unwrap().contains("hello"));
}

#[tokio::test]
async fn read_before_write_still_protects_a_file_inside_the_sandbox() {
    let sandbox = Arc::new(MemSandbox::default());
    let (ws, guard) = tools(&sandbox);
    sandbox.put("/workspace/existing.txt", b"original");

    let write = orca_harness_tools::WriteFileTool::new(ws.clone()).guard(guard.clone());

    // Unread: refused, exactly as on the host. Without `stat` on the
    // trait this check would have silently passed and the file would be
    // gone — protection that looks present and does nothing.
    let error = write
        .call(
            serde_json::json!({ "path": "existing.txt", "content": "clobbered" }),
            &ctx("write_file"),
        )
        .await
        .expect_err("overwriting an unread file must be refused");
    assert!(error.to_string().contains("has not been read"));
    assert_eq!(
        sandbox.get("/workspace/existing.txt").as_deref(),
        Some(&b"original"[..]),
        "the refused write must not have happened"
    );

    // Read it, then the same write is allowed.
    orca_harness_tools::ReadFileTool::new(ws.clone())
        .guard(guard.clone())
        .call(
            serde_json::json!({ "path": "existing.txt" }),
            &ctx("read_file"),
        )
        .await
        .expect("read");
    write
        .call(
            serde_json::json!({ "path": "existing.txt", "content": "deliberate" }),
            &ctx("write_file"),
        )
        .await
        .expect("write after read");
    assert_eq!(
        sandbox.get("/workspace/existing.txt").as_deref(),
        Some(&b"deliberate"[..])
    );
}

#[tokio::test]
async fn listing_and_searching_read_the_sandbox_tree() {
    let sandbox = Arc::new(MemSandbox::default());
    let (ws, _) = tools(&sandbox);
    sandbox.put("/workspace/src/main.rs", b"fn main() { todo!() }");
    sandbox.put("/workspace/README.md", b"nothing to see");

    let list = orca_harness_tools::ListDirTool::new(ws.clone())
        .call(serde_json::json!({ "path": "." }), &ctx("list_dir"))
        .await
        .expect("list");
    let names: Vec<&str> = list["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"src"));
    assert!(names.contains(&"README.md"));

    let grep = orca_harness_tools::GrepTool::new(ws.clone())
        .call(serde_json::json!({ "query": "todo!" }), &ctx("grep"))
        .await
        .expect("grep");
    let hits = grep["matches"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "got {hits:?}");
    assert_eq!(hits[0]["path"], "src/main.rs");

    let glob = orca_harness_tools::GlobTool::new(ws)
        .call(serde_json::json!({ "pattern": "*.rs" }), &ctx("glob"))
        .await
        .expect("glob");
    assert_eq!(
        glob["matches"].as_array().unwrap(),
        &vec![serde_json::json!("src/main.rs")]
    );
}

#[tokio::test]
async fn path_escapes_are_still_refused_under_a_sandbox() {
    let sandbox = Arc::new(MemSandbox::default());
    let (ws, _) = tools(&sandbox);

    let read = orca_harness_tools::ReadFileTool::new(ws);
    for bad in ["../etc/passwd", "/etc/passwd", "a/../../b"] {
        let error = read
            .call(serde_json::json!({ "path": bad }), &ctx("read_file"))
            .await
            .expect_err("escape must be refused");
        let message = error.to_string();
        assert!(
            message.contains("escapes") || message.contains("must be relative"),
            "{bad}: {message}"
        );
    }
    assert!(sandbox.files.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_assembled_sandbox_set_has_no_host_backed_tool() {
    let sandbox = Arc::new(MemSandbox::default());
    let tools =
        orca_harness_tools::core_tools_in_sandbox(sandbox.clone(), "/workspace").expect("assemble");

    let names: Vec<String> = tools.iter().map(|t| t.schema().name).collect();
    assert!(names.contains(&"shell".to_string()));
    assert!(names.contains(&"read_file".to_string()));
    assert!(names.contains(&"grep".to_string()));

    // Deliberately absent until a sandbox-backed spawner exists: their
    // host versions would put a local process in a set that must have
    // none. Silence here would be the bug.
    for stateful in ["process", "bun_repl", "py_kernel"] {
        assert!(
            !names.contains(&stateful.to_string()),
            "{stateful} has no sandbox backend yet and must not be registered"
        );
    }

    // Every tool in the set actually reaches the provider.
    let write = tools
        .iter()
        .find(|t| t.schema().name == "write_file")
        .expect("write_file");
    write
        .call(
            serde_json::json!({ "path": "from-set.txt", "content": "x" }),
            &ctx("write_file"),
        )
        .await
        .expect("write");
    assert_eq!(
        sandbox.get("/workspace/from-set.txt").as_deref(),
        Some(&b"x"[..])
    );
}

/// A provider with no file API cannot back the file tools. Returning a
/// set rooted on the host instead would be the silent failure.
#[tokio::test]
async fn assembly_refuses_a_provider_without_a_file_api() {
    struct NoFiles;

    #[async_trait]
    impl Sandbox for NoFiles {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                sessions: false,
                file_api: false,
                network_policy: false,
            }
        }
        async fn exec(&self, _: ExecRequest) -> Result<ExecOutput, SandboxError> {
            Ok(ExecOutput::default())
        }
        async fn spawn(&self, _: SpawnRequest) -> Result<(Box<dyn Session>, Output), SandboxError> {
            unreachable!()
        }
        async fn read_file(&self, _: &str) -> Result<Vec<u8>, SandboxError> {
            unreachable!()
        }
        async fn write_file(&self, _: &str, _: &[u8], _: FileMode) -> Result<(), SandboxError> {
            unreachable!()
        }
        async fn list_dir(&self, _: &str) -> Result<Vec<Entry>, SandboxError> {
            unreachable!()
        }
        async fn stat(&self, _: &str) -> Result<Option<Stat>, SandboxError> {
            unreachable!()
        }
        async fn shutdown(&self) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    match orca_harness_tools::core_tools_in_sandbox(Arc::new(NoFiles), "/workspace") {
        Err(SandboxError::Unsupported { capability, .. }) => {
            assert!(capability.contains("file API"));
        }
        Err(other) => panic!("wrong refusal: {other}"),
        Ok(_) => panic!("a provider without a file API must not yield a tool set"),
    }
}
