//! Headless approvals over stdio (`--approvals stdio`): a gated tool call
//! emits an `approval_request` NDJSON line on stdout and waits for the
//! caller's decision, one JSON object per line on stdin:
//!
//! ```text
//! {"type":"approval_request","id":"approval-1","tool_call_id":"c1","tool_name":"shell","input":{...}}
//! {"type":"approval","id":"approval-1","decision":"allow"}        <- stdin
//! ```
//!
//! Tool calls run concurrently, so several requests may be open at once;
//! one reader thread routes each decision to its request by id. EOF on
//! stdin denies every open request and every later one. The reader is a
//! plain thread, not a runtime task: a blocking read inside the runtime
//! would keep the process alive after the run while the caller holds
//! stdin open.

use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use orca_harness_core::{Extension, ExtensionError, Subscriptions, ToolCall, ToolDecision};
use serde::Deserialize;
use tokio::sync::oneshot;

use crate::approval::GATED_TOOLS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
    /// Allow this tool for the rest of the run. Never saved.
    Always,
}

#[derive(Deserialize)]
struct Answer {
    #[serde(rename = "type")]
    kind: String,
    id: String,
    decision: Decision,
}

#[derive(Default)]
struct Pending {
    open: HashMap<String, oneshot::Sender<Decision>>,
    closed: bool,
}

type Emit = Arc<dyn Fn(String) + Send + Sync>;

#[derive(Clone)]
pub struct StdioApprovals {
    pending: Arc<Mutex<Pending>>,
    always: Arc<Mutex<HashSet<String>>>,
    next_id: Arc<AtomicU64>,
    emit: Emit,
    /// `--auto-approve` or yolo already admitted gated tools; only the
    /// read-only shell still asks.
    admit_gated: bool,
    /// `--read-only`: shell is allowed only through a decision here, and
    /// never through an `always` grant.
    read_only: bool,
}

impl StdioApprovals {
    /// Read decisions from the process's stdin and print requests on stdout.
    pub fn from_process(admit_gated: bool, read_only: bool) -> Self {
        let gate = Self::new(Arc::new(|line| println!("{line}")), admit_gated, read_only);
        gate.spawn_reader(std::io::BufReader::new(std::io::stdin()));
        gate
    }

    fn new(emit: Emit, admit_gated: bool, read_only: bool) -> Self {
        Self {
            pending: Arc::default(),
            always: Arc::default(),
            next_id: Arc::new(AtomicU64::new(1)),
            emit,
            admit_gated,
            read_only,
        }
    }

    fn spawn_reader(&self, reader: impl BufRead + Send + 'static) {
        let pending = self.pending.clone();
        std::thread::Builder::new()
            .name("orcacode-approvals".into())
            .spawn(move || read_decisions(reader, &pending))
            .expect("spawn approval reader");
    }

    fn needs_answer(&self, call: &ToolCall) -> bool {
        if self.read_only && call.name == "shell" {
            return true;
        }
        if self.admit_gated || !GATED_TOOLS.contains(&call.name.as_str()) {
            return false;
        }
        !self.always.lock().unwrap().contains(&call.name)
    }
}

fn read_decisions(reader: impl BufRead, pending: &Mutex<Pending>) {
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Answer>(&line) {
            Ok(answer) if answer.kind == "approval" => answer,
            Ok(answer) => {
                eprintln!("warning: ignored stdin message of type {:?}", answer.kind);
                continue;
            }
            Err(error) => {
                eprintln!("warning: ignored malformed approval line: {error}");
                continue;
            }
        };
        let sender = pending.lock().unwrap().open.remove(&answer.id);
        match sender {
            Some(sender) => {
                let _ = sender.send(answer.decision);
            }
            None => eprintln!(
                "warning: approval for unknown or already answered id {:?}",
                answer.id
            ),
        }
    }
    // EOF or a read error: nobody is left to answer.
    let mut pending = pending.lock().unwrap();
    pending.closed = true;
    for (_, sender) in pending.open.drain() {
        let _ = sender.send(Decision::Deny);
    }
}

#[async_trait]
impl Extension for StdioApprovals {
    fn name(&self) -> &str {
        "stdio-approvals"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().before_tool()
    }

    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        if !self.needs_answer(call) {
            return Ok(ToolDecision::Continue);
        }
        let id = format!("approval-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (respond, answer) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.closed {
                return Ok(deny("stdin is closed, so nobody can approve it"));
            }
            pending.open.insert(id.clone(), respond);
        }
        (self.emit)(
            serde_json::json!({
                "type": "approval_request",
                "id": id,
                "tool_call_id": call.id,
                "tool_name": call.name,
                "input": call.arguments,
            })
            .to_string(),
        );
        Ok(match answer.await {
            Ok(Decision::Allow) => ToolDecision::Continue,
            Ok(Decision::Always) => {
                if !(self.read_only && call.name == "shell") {
                    self.always.lock().unwrap().insert(call.name.clone());
                }
                ToolDecision::Continue
            }
            Ok(Decision::Deny) => deny("the caller denied it"),
            Err(_) => deny("stdin is closed, so nobody can approve it"),
        })
    }
}

fn deny(why: &str) -> ToolDecision {
    ToolDecision::Deny {
        reason: format!("This tool call was not run: {why}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::io::{BufReader, Read};
    use std::sync::mpsc;

    /// A stdin the test writes to line by line; dropping the sender is EOF.
    struct Feed(mpsc::Receiver<String>, Vec<u8>);

    impl Read for Feed {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.1.is_empty() {
                match self.0.recv() {
                    Ok(line) => self.1 = format!("{line}\n").into_bytes(),
                    Err(_) => return Ok(0),
                }
            }
            let n = buf.len().min(self.1.len());
            buf[..n].copy_from_slice(&self.1[..n]);
            self.1.drain(..n);
            Ok(n)
        }
    }

    fn gate(
        admit: bool,
        read_only: bool,
    ) -> (StdioApprovals, mpsc::Sender<String>, mpsc::Receiver<Value>) {
        let (out_tx, out_rx) = mpsc::channel();
        let out_tx = Mutex::new(out_tx);
        let gate = StdioApprovals::new(
            Arc::new(move |line: String| {
                out_tx
                    .lock()
                    .unwrap()
                    .send(serde_json::from_str(&line).unwrap())
                    .unwrap()
            }),
            admit,
            read_only,
        );
        let (in_tx, in_rx) = mpsc::channel();
        gate.spawn_reader(BufReader::new(Feed(in_rx, Vec::new())));
        (gate, in_tx, out_rx)
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({"command": "ls"}),
        }
    }

    fn answer(id: &Value, decision: &str) -> String {
        json!({"type": "approval", "id": id, "decision": decision}).to_string()
    }

    fn denied(decision: &ToolDecision) -> bool {
        matches!(decision, ToolDecision::Deny { .. })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_requests_are_routed_by_id() {
        let (gate, input, output) = gate(false, false);
        let first = tokio::spawn({
            let gate = gate.clone();
            async move { gate.before_tool(&call("c1", "shell")).await.unwrap() }
        });
        let second = tokio::spawn({
            let gate = gate.clone();
            async move { gate.before_tool(&call("c2", "write_file")).await.unwrap() }
        });
        let mut requests = [
            output
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap(),
            output
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap(),
        ];
        requests.sort_by_key(|r| r["tool_call_id"].as_str().unwrap().to_string());
        assert_eq!(requests[0]["type"], "approval_request");
        assert_eq!(requests[0]["tool_name"], "shell");
        assert_eq!(requests[0]["input"], json!({"command": "ls"}));
        // Answer out of order: the second request first.
        input.send("not json".into()).unwrap();
        input.send(answer(&requests[1]["id"], "deny")).unwrap();
        input.send(answer(&requests[0]["id"], "allow")).unwrap();
        assert!(matches!(first.await.unwrap(), ToolDecision::Continue));
        assert!(denied(&second.await.unwrap()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn always_lasts_for_the_run_and_ungated_tools_never_ask() {
        let (gate, input, output) = gate(false, false);
        let pending = tokio::spawn({
            let gate = gate.clone();
            async move { gate.before_tool(&call("c1", "shell")).await.unwrap() }
        });
        let request = output
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        input.send(answer(&request["id"], "always")).unwrap();
        assert!(matches!(pending.await.unwrap(), ToolDecision::Continue));
        let again = gate.before_tool(&call("c2", "shell")).await.unwrap();
        assert!(matches!(again, ToolDecision::Continue));
        let read = gate.before_tool(&call("c3", "read_file")).await.unwrap();
        assert!(matches!(read, ToolDecision::Continue));
        assert!(output.try_recv().is_err(), "no further requests expected");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn eof_denies_open_and_later_requests() {
        let (gate, input, output) = gate(false, false);
        let pending = tokio::spawn({
            let gate = gate.clone();
            async move { gate.before_tool(&call("c1", "shell")).await.unwrap() }
        });
        output
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        drop(input);
        assert!(denied(&pending.await.unwrap()));
        // The reader has finished; a later request is refused without asking.
        for _ in 0..50 {
            if gate.pending.lock().unwrap().closed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(denied(
            &gate.before_tool(&call("c2", "edit_file")).await.unwrap()
        ));
        assert!(output.try_recv().is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_only_shell_asks_every_time_even_when_gated_tools_are_admitted() {
        let (gate, input, output) = gate(true, true);
        assert!(matches!(
            gate.before_tool(&call("c0", "write_file")).await.unwrap(),
            ToolDecision::Continue
        ));
        for id in ["c1", "c2"] {
            let pending = tokio::spawn({
                let gate = gate.clone();
                async move { gate.before_tool(&call(id, "shell")).await.unwrap() }
            });
            let request = output
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            assert_eq!(request["tool_call_id"], id);
            input.send(answer(&request["id"], "always")).unwrap();
            assert!(matches!(pending.await.unwrap(), ToolDecision::Continue));
        }
    }
}
