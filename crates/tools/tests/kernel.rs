//! Kernel tool against a real python3. Every test skips (with a notice)
//! when python3 is absent.

use serde_json::json;

use orca_harness_core::{CancellationToken, Concurrency, Tool, ToolContext};
use orca_harness_tools::KernelTool;

fn ctx() -> ToolContext {
    ToolContext {
        call_id: "t".into(),
        tool_name: "kernel".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

macro_rules! require_python {
    () => {
        if !python3_available() {
            eprintln!("skipping: python3 not found on PATH");
            return;
        }
    };
}

#[tokio::test]
async fn state_persists_across_calls() {
    require_python!();
    let k = KernelTool::new();
    let out = k.call(json!({"code": "x = 41"}), &ctx()).await.unwrap();
    assert_eq!(out["state"], "ok");
    assert_eq!(out["output"], "");
    let out = k.call(json!({"code": "print(x + 1)"}), &ctx()).await.unwrap();
    assert_eq!(out["state"], "ok");
    assert_eq!(out["output"].as_str().unwrap().trim(), "42");
}

#[tokio::test]
async fn multiline_code_runs_verbatim() {
    require_python!();
    let k = KernelTool::new();
    // Blank lines inside a def are exactly what wedged the interactive REPL.
    let code = "def double(n):\n\n    return n * 2\n\nprint(double(21))";
    let out = k.call(json!({"code": code}), &ctx()).await.unwrap();
    assert_eq!(out["state"], "ok");
    assert_eq!(out["output"].as_str().unwrap().trim(), "42");
}

#[tokio::test]
async fn errors_report_traceback_and_preserve_state() {
    require_python!();
    let k = KernelTool::new();
    k.call(json!({"code": "kept = 'alive'"}), &ctx()).await.unwrap();
    let out = k.call(json!({"code": "1 / 0"}), &ctx()).await.unwrap();
    assert_eq!(out["state"], "error");
    assert!(out["traceback"]
        .as_str()
        .unwrap()
        .contains("ZeroDivisionError"));
    // An exception must not cost the session its state.
    let out = k.call(json!({"code": "print(kept)"}), &ctx()).await.unwrap();
    assert_eq!(out["output"].as_str().unwrap().trim(), "alive");
}

#[tokio::test]
async fn stderr_is_captured_in_order() {
    require_python!();
    let k = KernelTool::new();
    let out = k
        .call(
            json!({"code": "import sys\nprint('a')\nprint('b', file=sys.stderr)\nprint('c')"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["output"].as_str().unwrap().trim(), "a\nb\nc");
}

#[test]
fn kernel_calls_are_serial() {
    let k = KernelTool::new();
    assert_eq!(k.concurrency(&json!({"code": "1"})), Concurrency::Serial);
}
