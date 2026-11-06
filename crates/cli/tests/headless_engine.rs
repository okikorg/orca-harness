//! End-to-end checks of the headless engine contract a host drives:
//! `orcacode -p ... --json` against a scripted OpenAI-compatible server and
//! a real stdio MCP server whose executable path contains a space.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

static NEXT: AtomicU64 = AtomicU64::new(1);

/// One scripted model reply.
#[derive(Clone)]
enum Reply {
    Text(&'static str),
    Call(&'static str, Value),
}

impl Reply {
    fn sse(&self, step: usize) -> String {
        let chunks = match self {
            Reply::Text(text) => vec![
                json!({"choices": [{"index": 0, "delta": {"content": text}}]}),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            ],
            Reply::Call(name, args) => vec![
                json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": 0,
                    "id": format!("call_{step}"),
                    "type": "function",
                    "function": {"name": name, "arguments": args.to_string()},
                }]}}]}),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            ],
        };
        let mut body = chunks
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect::<String>();
        body.push_str("data: [DONE]\n\n");
        body
    }
}

/// A chat-completions server that answers requests with `script` in order
/// (the last reply repeats) and records every request body.
struct FakeModel {
    url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
}

impl FakeModel {
    fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let recorded = bodies.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some(body) = read_request(&mut stream) else {
                    continue;
                };
                let step = {
                    let mut bodies = recorded.lock().unwrap();
                    bodies.push(body);
                    bodies.len() - 1
                };
                let reply = script[step.min(script.len() - 1)].sse(step);
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    reply.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        Self { url, bodies }
    }

    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Value> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
        let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&raw[..split]).to_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .map(|value| value.trim().parse().unwrap())
            .unwrap_or(0);
        if raw.len() >= split + 4 + length {
            return serde_json::from_slice(&raw[split + 4..split + 4 + length]).ok();
        }
    }
}

/// A minimal stdio MCP server with one `echo` tool that reports its argv
/// and one environment variable, installed under a directory with a space.
const MCP_SERVER: &str = r#"#!/usr/bin/env python3
import json, os, sys
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    method = msg.get("method")
    if method == "initialize":
        result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                  "serverInfo": {"name": "fake", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "Echo text back",
                  "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}
    elif method == "tools/call":
        text = msg["params"]["arguments"].get("text", "")
        report = "echo:%s argv:%s env:%s" % (text, json.dumps(sys.argv[1:]), os.environ.get("FAKE_MCP_VALUE"))
        result = {"content": [{"type": "text", "text": report}]}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "no"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
"#;

struct Fixture {
    root: PathBuf,
    config: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "orcacode-headless-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        Self {
            config: root.join("config"),
            workspace,
            root,
        }
    }

    fn mcp_server(&self) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.root.join("mcp tools");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fake server.py");
        std::fs::write(&path, MCP_SERVER).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn command(&self, model: &FakeModel, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_orcacode"));
        command
            .args([
                "--base-url",
                &model.url,
                "--model",
                "fake",
                "--no-prompt-cache",
            ])
            .args(args)
            .current_dir(&self.workspace)
            .env("ORCA_CONFIG_DIR", &self.config)
            .env("HOME", &self.root)
            .env_remove("ORCA_MODEL")
            .env_remove("ORCA_BASE_URL")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Run to completion with `stdin` as the whole input.
    fn run(&self, model: &FakeModel, args: &[&str], stdin: &str) -> (Vec<Value>, String, i32) {
        let mut child = self
            .command(model, args)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        (
            ndjson(&String::from_utf8_lossy(&output.stdout)),
            String::from_utf8_lossy(&output.stderr).into_owned(),
            output.status.code().unwrap_or(-1),
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn ndjson(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|_| panic!("not JSON: {line}")))
        .collect()
}

fn event<'a>(events: &'a [Value], kind: &str) -> &'a Value {
    events
        .iter()
        .find(|event| event["type"] == kind)
        .unwrap_or_else(|| panic!("no {kind} event in {events:#?}"))
}

fn have_python() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

#[test]
fn stdin_prompt_run_mcp_config_session_id_and_resume() {
    if !have_python() {
        eprintln!("skipped: python3 is required for the fake MCP server");
        return;
    }
    let fixture = Fixture::new("mcp");
    let server = fixture.mcp_server();
    let mcp_config = json!({
        "srv": {
            "command": server,
            "args": ["first arg", "--flag"],
            "env": {"FAKE_MCP_VALUE": "value with spaces"},
        }
    })
    .to_string();
    let model = FakeModel::start(vec![
        Reply::Call("mcp_select_tool", json!({"name": "mcp__srv__echo"})),
        Reply::Call("mcp__srv__echo", json!({"text": "hello"})),
        Reply::Text("done"),
    ]);
    let prompt = "-a prompt that starts with a dash\nand has two lines";
    let (events, stderr, code) = fixture.run(
        &model,
        &["-p", "-", "--json", "--mcp-config", &mcp_config],
        prompt,
    );
    assert_eq!(code, 0, "{stderr}");
    let metadata = event(&events, "metadata");
    let session_id = metadata["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    assert_eq!(metadata["resumed"], false);
    assert_eq!(metadata["mcpServers"], json!(["srv"]));
    let result = events
        .iter()
        .find(|e| e["type"] == "tool_result" && e["tool_name"] == "mcp__srv__echo")
        .unwrap_or_else(|| panic!("no MCP result: {events:#?}\n{stderr}"));
    let output = result["output"].to_string();
    assert_eq!(result["is_error"], false, "{output}");
    assert!(output.contains("echo:hello"), "{output}");
    assert!(
        output.contains("first arg") && output.contains("--flag"),
        "{output}"
    );
    assert!(output.contains("env:value with spaces"), "{output}");
    let summary = event(&events, "summary");
    assert_eq!(summary["status"], "success");
    assert_eq!(summary["sessionId"], session_id);
    let first_request = model.bodies()[0].to_string();
    assert!(first_request.contains("-a prompt that starts with a dash"));
    // Per-run servers are never saved.
    let saved = std::fs::read_to_string(fixture.config.join("config.json")).unwrap_or_default();
    assert!(!saved.contains("srv"), "{saved}");

    let model = FakeModel::start(vec![Reply::Text("again")]);
    let (events, stderr, code) = fixture.run(
        &model,
        &["--resume", &session_id, "-p", "continue", "--json"],
        "",
    );
    assert_eq!(code, 0, "{stderr}");
    let metadata = event(&events, "metadata");
    assert_eq!(metadata["sessionId"], session_id);
    assert_eq!(metadata["resumed"], true);
    assert!(model.bodies()[0].to_string().contains("starts with a dash"));
}

#[test]
fn no_session_reports_a_null_id_and_max_steps_has_its_own_status() {
    let fixture = Fixture::new("steps");
    let model = FakeModel::start(vec![Reply::Call(
        "read_file",
        json!({"path": "missing.txt"}),
    )]);
    let (events, stderr, code) = fixture.run(
        &model,
        &["-p", "loop", "--json", "--no-session", "--max-steps", "1"],
        "",
    );
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(event(&events, "metadata")["sessionId"], Value::Null);
    let summary = event(&events, "summary");
    assert_eq!(summary["status"], "max_steps");
    assert_eq!(summary["sessionId"], Value::Null);
}

#[test]
fn read_only_denies_writes_even_with_auto_approve() {
    let fixture = Fixture::new("readonly");
    let model = FakeModel::start(vec![
        Reply::Call(
            "write_file",
            json!({"path": "docs/plan/x.md", "content": "x"}),
        ),
        Reply::Text("ok"),
    ]);
    let (events, stderr, code) = fixture.run(
        &model,
        &[
            "-p",
            "write",
            "--json",
            "--read-only",
            "--auto-approve",
            "--plan",
        ],
        "",
    );
    assert_eq!(code, 0, "{stderr}");
    let result = event(&events, "tool_result");
    assert_eq!(result["is_error"], true);
    assert!(
        result["output"].to_string().contains("Read-only"),
        "{result}"
    );
    assert!(!fixture.workspace.join("docs/plan/x.md").exists());
}

#[test]
fn images_are_attached_to_the_user_message() {
    let fixture = Fixture::new("image");
    let image = fixture.root.join("shot one.png");
    std::fs::write(&image, b"\x89PNG").unwrap();
    let model = FakeModel::start(vec![Reply::Text("seen")]);
    let (_, stderr, code) = fixture.run(
        &model,
        &[
            "-p",
            "look",
            "--json",
            "--no-session",
            "--image",
            image.to_str().unwrap(),
        ],
        "",
    );
    assert_eq!(code, 0, "{stderr}");
    let request = model.bodies()[0].to_string();
    assert!(
        request.contains("data:image/png;base64,iVBORw=="),
        "{request}"
    );

    let (_, stderr, code) = fixture.run(
        &model,
        &["-p", "look", "--no-session", "--image", "missing.png"],
        "",
    );
    assert_eq!(code, 1);
    assert!(stderr.contains("--image missing.png"), "{stderr}");
}

#[test]
fn stdio_approvals_round_trip_and_exit_while_stdin_stays_open() {
    let fixture = Fixture::new("approvals");
    let prompt_file = fixture.root.join("prompt one.txt");
    std::fs::write(&prompt_file, "make a file").unwrap();
    let model = FakeModel::start(vec![
        Reply::Call("write_file", json!({"path": "a.txt", "content": "allowed"})),
        Reply::Call("write_file", json!({"path": "b.txt", "content": "denied"})),
        Reply::Text("finished"),
    ]);
    let mut child = fixture
        .command(
            &model,
            &[
                "--prompt-file",
                prompt_file.to_str().unwrap(),
                "--json",
                "--no-session",
                "--approvals",
                "stdio",
            ],
        )
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let mut events = Vec::new();
    let mut answers = ["allow", "deny"].into_iter();
    for line in stdout.lines() {
        let event: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if event["type"] == "approval_request" {
            assert_eq!(event["tool_name"], "write_file");
            assert!(event["tool_call_id"].is_string());
            let decision = answers.next().expect("only two requests");
            writeln!(
                stdin,
                "{}",
                json!({"type": "approval", "id": event["id"], "decision": decision})
            )
            .unwrap();
        }
        events.push(event);
    }
    // stdout closed while stdin is still open: the process must exit on its own.
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "orcacode hung with stdin open");
        std::thread::sleep(Duration::from_millis(50));
    };
    drop(stdin);
    assert!(status.success());
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("a.txt")).unwrap(),
        "allowed"
    );
    assert!(!fixture.workspace.join("b.txt").exists());
    assert_eq!(event(&events, "summary")["status"], "success");
}

#[test]
fn flag_combinations_that_cannot_work_are_rejected() {
    let fixture = Fixture::new("flags");
    let model = FakeModel::start(vec![Reply::Text("unused")]);
    for (args, expected) in [
        (
            &["-p", "-", "--approvals", "stdio", "--json"][..],
            "--prompt-file",
        ),
        (&["-p", "x", "--approvals", "stdio"][..], "requires --json"),
        (
            &[
                "-p",
                "x",
                "--bare",
                "--mcp-config",
                r#"{"a":{"command":"x"}}"#,
            ][..],
            "--bare skips MCP",
        ),
        (&["--read-only"][..], "require headless"),
        (
            &["-p", "x", "--prompt-file", "f"][..],
            "give the prompt once",
        ),
    ] {
        let (_, stderr, code) = fixture.run(&model, args, "");
        assert_eq!(code, 1, "{args:?}");
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }
    let (_, stderr, code) = fixture.run(&model, &["-p", "-", "--no-session"], "");
    assert_eq!(code, 1);
    assert!(stderr.contains("stdin is empty"), "{stderr}");
    assert!(model.bodies().is_empty());
}
