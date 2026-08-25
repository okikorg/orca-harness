use std::path::PathBuf;

use serde_json::json;

use orca_harness_core::{CancellationToken, Concurrency, Tool, ToolContext};

use super::{clean_stdout, strip_ansi, BunReplTool, TempSource};
use crate::BackgroundStats;

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
    let path = source.0.clone();
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
