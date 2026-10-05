use std::path::PathBuf;

use serde_json::json;

use orca_harness_core::{CancellationToken, Concurrency, Tool, ToolContext};

use super::rpc::Rpc;
use super::{clean_stdout, output::strip_ansi, BunReplTool, TempSource};
use crate::{BackgroundStats, ProgrammaticTools, ToolDispatch};

fn ctx() -> ToolContext {
    ToolContext {
        call_id: "t".into(),
        tool_name: "bun_repl".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

fn bun_available() -> bool {
    std::process::Command::new("bun")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

macro_rules! require_bun {
    () => {
        if !bun_available() {
            eprintln!("skipping: bun not found on PATH");
            return;
        }
    };
}

fn temp_workspace(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("orca-bun-repl-test-{}-{label}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn typescript_state_persists_across_calls() {
    require_bun!();
    let repl = BunReplTool::new();
    let out = repl
        .call(
            json!({"code": "const values: number[] = [12, 18, 25]"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["state"], "ok");
    let out = repl
        .call(
            json!({"code": "console.log(values.reduce((a, b) => a + b, 0))"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["output"].as_str().unwrap().trim(), "55");
}

#[tokio::test]
async fn multiline_typescript_and_top_level_await_work() {
    require_bun!();
    let repl = BunReplTool::new();
    let code = "function percentile(values: number[], p: number): number {\n\
                \x20 const sorted = [...values].sort((a, b) => a - b)\n\
                \x20 return sorted[Math.ceil(p * sorted.length) - 1]\n\
                }\n\
                await Bun.sleep(10)\n\
                console.log(percentile([12, 18, 25], 0.95))";
    let out = repl.call(json!({"code": code}), &ctx()).await.unwrap();
    assert_eq!(out["state"], "ok");
    assert_eq!(out["output"].as_str().unwrap().trim(), "25");
}

#[tokio::test]
async fn imports_resolve_from_the_working_directory() {
    require_bun!();
    let dir = temp_workspace("imports");
    std::fs::write(dir.join("fixture.ts"), "export const answer = 42\n").unwrap();
    let repl = BunReplTool::new().working_dir(dir.to_string_lossy());
    let out = repl
        .call(
            json!({"code": "import { answer } from './fixture.ts'\nconsole.log(answer)"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["state"], "ok");
    assert_eq!(out["output"].as_str().unwrap().trim(), "42");
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn errors_are_reported_without_losing_state() {
    require_bun!();
    let repl = BunReplTool::new();
    repl.call(json!({"code": "const kept = 'alive'"}), &ctx())
        .await
        .unwrap();
    let out = repl
        .call(json!({"code": "JSON.parse('not-json')"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["state"], "error");
    assert!(out["output"].as_str().unwrap().contains("SyntaxError"));
    let out = repl
        .call(json!({"code": "console.log(kept)"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["output"].as_str().unwrap().trim(), "alive");
}

#[tokio::test]
async fn stderr_is_returned_separately() {
    require_bun!();
    let repl = BunReplTool::new();
    let out = repl
        .call(
            json!({"code": "console.log('out'); console.error('err')"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["state"], "ok");
    assert_eq!(out["output"].as_str().unwrap().trim(), "out");
    assert_eq!(out["stderr"].as_str().unwrap().trim(), "err");
}

#[tokio::test]
async fn high_volume_output_is_truncated_without_losing_completion() {
    require_bun!();
    let repl = BunReplTool::new().max_output_bytes(1024);
    let out = repl
        .call(
            json!({"code": "for (let i = 0; i < 20_000; i++) console.log('xxxxxxxxxxxxxxxx')"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["state"], "ok", "{out}");
    assert!(out["droppedBytes"].as_u64().unwrap() > 0);
    assert!(out["output"].as_str().unwrap().len() <= 1024);
}

#[test]
fn calls_serialize_only_against_the_same_repl() {
    let repl = BunReplTool::new();
    assert_eq!(
        repl.concurrency(&json!({"code": "1"})),
        Concurrency::Keyed("bun_repl".into())
    );
}

#[tokio::test]
async fn timeout_kills_repl_and_next_call_restarts_fresh() {
    require_bun!();
    let repl = BunReplTool::new();
    repl.call(json!({"code": "const oldState = 7"}), &ctx())
        .await
        .unwrap();
    let out = repl
        .call(
            json!({"code": "await Bun.sleep(60_000)", "timeoutMs": 100}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["state"], "timeout");
    let out = repl
        .call(json!({"code": "console.log(typeof oldState)"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["restarted"], true);
    assert_eq!(out["output"].as_str().unwrap().trim(), "undefined");
}

#[tokio::test]
async fn reset_discards_state_without_a_second_restart_notice() {
    require_bun!();
    let repl = BunReplTool::new();
    repl.call(json!({"code": "const oldState = 7"}), &ctx())
        .await
        .unwrap();
    let out = repl.call(json!({"action": "reset"}), &ctx()).await.unwrap();
    assert_eq!(out["restarted"], true);
    let out = repl
        .call(json!({"code": "console.log(typeof oldState)"}), &ctx())
        .await
        .unwrap();
    assert!(out.get("restarted").is_none());
    assert_eq!(out["output"].as_str().unwrap().trim(), "undefined");
}

#[tokio::test]
async fn crash_is_reported_and_next_call_restarts() {
    require_bun!();
    let repl = BunReplTool::new();
    let error = repl
        .call(json!({"code": "process.exit(3)"}), &ctx())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("exited before completing"));
    let out = repl
        .call(json!({"code": "console.log('fresh')"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["restarted"], true);
    assert_eq!(out["output"].as_str().unwrap().trim(), "fresh");
}

#[tokio::test]
async fn repl_stats_track_liveness() {
    require_bun!();
    let stats = BackgroundStats::new();
    {
        let repl = BunReplTool::new().stats(stats.clone());
        assert_eq!(stats.bun_repls(), 0);
        repl.call(json!({"code": "const a = 1"}), &ctx())
            .await
            .unwrap();
        assert_eq!(stats.bun_repls(), 1);
        repl.call(json!({"action": "reset"}), &ctx()).await.unwrap();
        assert_eq!(stats.bun_repls(), 0);
        repl.call(json!({"code": "const liveAgain = true"}), &ctx())
            .await
            .unwrap();
        assert_eq!(stats.bun_repls(), 1);
    }
    assert_eq!(stats.bun_repls(), 0);
}

#[test]
fn terminal_redraws_and_banners_are_removed() {
    let raw = b"Welcome to Bun v1.3.14\r\n\r\x1b[2K> c\r\x1b[3C\r\x1b[2K> code\r\nanswer\r\n";
    assert_eq!(clean_stdout(raw), "answer");
}

#[test]
fn ansi_is_removed_without_losing_text() {
    assert_eq!(strip_ansi("a\x1b[31mred\x1b[0mz\r\n"), "aredz\n");
}

#[test]
fn temporary_source_is_private_and_removed_on_drop() {
    let source = TempSource::write("console.log('safe')", "\"marker\"").unwrap();
    let path = source.path.clone();
    assert!(path.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    drop(source);
    assert!(!path.exists());
}

fn dispatch(extensions: Vec<std::sync::Arc<dyn orca_harness_core::Extension>>) -> ToolDispatch {
    let echo =
        orca_harness_core::FnTool::new("echo", "", json!({}), |input, _| async move { Ok(input) });
    let tools: Vec<std::sync::Arc<dyn Tool>> = vec![
        std::sync::Arc::new(BunReplTool::new()),
        std::sync::Arc::new(echo),
    ];
    ToolDispatch::new(tools, extensions, ProgrammaticTools::new())
}

#[test]
fn a_dispatching_repl_is_serial_and_documents_the_tools_global() {
    let repl = std::sync::Arc::new(BunReplTool::new());
    let tool = repl.clone().with_dispatch(dispatch(Vec::new()));
    assert_eq!(tool.schema().name, "bun_repl");
    assert!(tool.schema().description.contains("tools.batch"));
    assert!(!repl.schema().description.contains("tools.batch"));
    assert_eq!(tool.concurrency(&json!({"code": "1"})), Concurrency::Serial);
    assert_eq!(
        repl.concurrency(&json!({"code": "1"})),
        Concurrency::Keyed("bun_repl".into())
    );
}

struct Offline;

#[async_trait::async_trait]
impl orca_harness_core::Extension for Offline {
    fn name(&self) -> &str {
        "gate"
    }

    fn subscriptions(&self) -> orca_harness_core::Subscriptions {
        orca_harness_core::Subscriptions::none().before_tool()
    }

    async fn before_tool(
        &self,
        _call: &orca_harness_core::ToolCall,
    ) -> Result<orca_harness_core::ToolDecision, orca_harness_core::ExtensionError> {
        Err(orca_harness_core::ExtensionError::new(
            "gate",
            "policy store offline",
        ))
    }
}

#[tokio::test]
async fn tools_requests_report_their_real_errors() {
    let plain = dispatch(Vec::new());
    let repl = BunReplTool::new();
    let rpc = Rpc::new(&plain, &repl);
    let listed = rpc.respond(&json!({"list": true}), &ctx()).await.unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["name"], "echo");
    let empty = rpc.respond(&json!({"calls": []}), &ctx()).await;
    assert_eq!(
        empty.unwrap_err(),
        "a tools batch holds 1 to 64 calls, not 0"
    );
    let wide: Vec<_> = (0..65).map(|_| json!({"name": "echo"})).collect();
    let wide = rpc.respond(&json!({"calls": wide}), &ctx()).await;
    assert_eq!(
        wide.unwrap_err(),
        "a tools batch holds 1 to 64 calls, not 65"
    );
    let invalid = rpc
        .respond(&json!({"calls": [{"arguments": {}}]}), &ctx())
        .await;
    assert!(invalid
        .unwrap_err()
        .starts_with("invalid tools request: missing field `name`"));

    let gated = dispatch(vec![std::sync::Arc::new(Offline)]);
    let rpc = Rpc::new(&gated, &repl);
    let failed = rpc
        .respond(&json!({"calls": [{"name": "echo"}]}), &ctx())
        .await;
    assert_eq!(
        failed.unwrap_err(),
        "extension error: gate: policy store offline"
    );
}

#[tokio::test]
async fn tools_frames_are_answered_in_sequence_through_the_response_file() {
    let dispatch = dispatch(Vec::new());
    let repl = BunReplTool::new();
    let mut rpc = Rpc::new(&dispatch, &repl);
    let prelude = rpc.prelude();
    let marker = prelude
        .split("const marker = ")
        .nth(1)
        .and_then(|rest| rest.split(';').next())
        .map(|marker| serde_json::from_str::<String>(marker).unwrap())
        .unwrap();
    let path: String = prelude
        .split("const path = ")
        .nth(1)
        .and_then(|rest| rest.split(';').next())
        .map(|path| serde_json::from_str(path).unwrap())
        .unwrap();

    let frame = r#"{"id":1,"calls":[{"name":"echo","arguments":{"n":7}}]}"#;
    let mut stderr = format!("warn: noise\n{marker}{frame}\n tail").into_bytes();
    let taken = rpc.take_frame(&mut stderr).unwrap();
    assert_eq!(taken, frame.as_bytes());
    assert_eq!(stderr, b"warn: noise\n tail");
    assert!(rpc.take_frame(&mut stderr).is_none());

    rpc.answer(&taken, &ctx()).await.unwrap();
    let response: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"][0]["output"], json!({"n": 7}));
    assert_eq!(response["result"][0]["call_id"], "t.ptc1");

    let skipped = rpc.answer(br#"{"id":3,"list":true}"#, &ctx()).await;
    assert_eq!(skipped.unwrap_err(), "out-of-sequence tools request");
    drop(rpc);
    assert!(!std::path::Path::new(&path).exists());
}

#[tokio::test]
async fn unawaited_calls_finish_and_stale_handles_reject_at_once() {
    require_bun!();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = {
        let seen = seen.clone();
        orca_harness_core::FnTool::new("record", "", json!({}), move |input, _| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(input["n"].as_i64().unwrap());
                Ok(json!(null))
            }
        })
    };
    let dispatch = ToolDispatch::new(
        [std::sync::Arc::new(record) as std::sync::Arc<dyn Tool>],
        [],
        ProgrammaticTools::new(),
    );
    let repl = std::sync::Arc::new(BunReplTool::new());
    let tool = repl.clone().with_dispatch(dispatch);

    let out = tool
        .call(
            json!({"code": "globalThis.keep = tools; tools.call('record', {n: 1}); tools.call('record', {n: 2}); 1"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["state"], "ok", "{out}");
    assert_eq!(*seen.lock().unwrap(), [1, 2]);

    // A saved handle from the earlier execution must fail fast, not poll
    // until the timeout kills the interpreter.
    let started = std::time::Instant::now();
    let out = tool
        .call(
            json!({"code": "try { await keep.call('record', {n: 3}); console.log('reached') } catch (e) { console.log(e.message) }", "timeoutMs": 10000}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["state"], "ok", "{out}");
    assert_eq!(
        out["output"].as_str().unwrap().trim(),
        "tools is only usable during the bun_repl call that installed it"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(out.get("restarted").is_none(), "{out}");
    assert_eq!(*seen.lock().unwrap(), [1, 2]);

    // Code that throws skips the epilogue; the end command still closes
    // the handle, so the pending call rejects instead of polling forever.
    let out = tool
        .call(
            json!({"code": "globalThis.late = tools; throw new Error('boom')"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["state"], "error", "{out}");
    let out = tool
        .call(
            json!({"code": "try { await late.list() } catch (e) { console.log(e.message) }"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(
        out["output"]
            .as_str()
            .unwrap()
            .contains("only usable during"),
        "{out}"
    );
}
