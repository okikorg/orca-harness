//! Kernel tool against a real python3. Every test skips (with a notice)
//! when python3 is absent.

use serde_json::json;

use orca_harness_core::{CancellationToken, Concurrency, Tool, ToolContext};
use orca_harness_tools::PyKernelTool;

fn ctx() -> ToolContext {
    ToolContext {
        call_id: "t".into(),
        tool_name: "pykernel".into(),
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
    let k = PyKernelTool::new();
    let out = k.call(json!({"code": "x = 41"}), &ctx()).await.unwrap();
    assert_eq!(out["state"], "ok");
    assert_eq!(out["output"], "");
    let out = k
        .call(json!({"code": "print(x + 1)"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["state"], "ok");
    assert_eq!(out["output"].as_str().unwrap().trim(), "42");
}

#[tokio::test]
async fn multiline_code_runs_verbatim() {
    require_python!();
    let k = PyKernelTool::new();
    // Blank lines inside a def are exactly what wedged the interactive REPL.
    let code = "def double(n):\n\n    return n * 2\n\nprint(double(21))";
    let out = k.call(json!({"code": code}), &ctx()).await.unwrap();
    assert_eq!(out["state"], "ok");
    assert_eq!(out["output"].as_str().unwrap().trim(), "42");
}

#[tokio::test]
async fn errors_report_traceback_and_preserve_state() {
    require_python!();
    let k = PyKernelTool::new();
    k.call(json!({"code": "kept = 'alive'"}), &ctx())
        .await
        .unwrap();
    let out = k.call(json!({"code": "1 / 0"}), &ctx()).await.unwrap();
    assert_eq!(out["state"], "error");
    assert!(out["traceback"]
        .as_str()
        .unwrap()
        .contains("ZeroDivisionError"));
    // An exception must not cost the session its state.
    let out = k
        .call(json!({"code": "print(kept)"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["output"].as_str().unwrap().trim(), "alive");
}

#[tokio::test]
async fn stderr_is_captured_in_order() {
    require_python!();
    let k = PyKernelTool::new();
    let out = k
        .call(
            json!({"code": "import sys\nprint('a')\nprint('b', file=sys.stderr)\nprint('c')"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["output"].as_str().unwrap().trim(), "a\nb\nc");
}

/// One kernel, one stdin: kernel calls serialize against each other in
/// call order — but only against each other, not the whole batch.
#[test]
fn kernel_calls_are_keyed_not_globally_exclusive() {
    let k = PyKernelTool::new();
    assert_eq!(
        k.concurrency(&json!({"code": "1"})),
        Concurrency::Keyed("pykernel".into())
    );
}

/// A slow kernel exec must not stall unrelated parallel calls: batch wall
/// clock tracks max(kernel, shells), not their sum.
#[tokio::test(flavor = "multi_thread")]
async fn kernel_exec_overlaps_unrelated_tools_in_one_batch() {
    require_python!();
    use std::sync::Arc;

    use orca_harness_core::testing::call;
    use orca_harness_core::{Dispatcher, ExtensionRegistry, ToolRegistry};
    use orca_harness_tools::ShellTool;

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(PyKernelTool::new()));
    tools.register(Arc::new(ShellTool::local()));

    let batch = vec![
        call(
            "k",
            "pykernel",
            json!({"code": "import time\ntime.sleep(0.4)\nprint('k')"}),
        ),
        call("s0", "shell", json!({"command": "sleep 0.4"})),
        call("s1", "shell", json!({"command": "sleep 0.4"})),
    ];
    let started = std::time::Instant::now();
    let results = Dispatcher::new()
        .execute(
            batch,
            &tools,
            &ExtensionRegistry::new(),
            &CancellationToken::new(),
            None,
            3,
        )
        .await
        .unwrap();
    let wall = started.elapsed();

    for r in &results {
        assert!(!r.is_error, "{} failed: {}", r.call_id, r.output);
    }
    assert!(
        wall < std::time::Duration::from_millis(700),
        "kernel must overlap the shells (~400ms), not exclude them (800ms+); took {wall:?}"
    );
}

#[tokio::test]
async fn timeout_kills_kernel_and_next_call_restarts_fresh() {
    require_python!();
    let k = PyKernelTool::new();
    k.call(json!({"code": "y = 7"}), &ctx()).await.unwrap();
    let out = k
        .call(
            json!({"code": "import time\ntime.sleep(60)", "timeoutMs": 500}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["state"], "timeout");
    // Fresh kernel: y is gone, and the restart is announced.
    let out = k
        .call(json!({"code": "print('y' in dir())"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["state"], "ok");
    assert_eq!(out["restarted"], true);
    assert_eq!(out["output"].as_str().unwrap().trim(), "False");
}

#[tokio::test]
async fn reset_discards_state_without_restart_notice_afterwards() {
    require_python!();
    let k = PyKernelTool::new();
    k.call(json!({"code": "z = 1"}), &ctx()).await.unwrap();
    let out = k.call(json!({"action": "reset"}), &ctx()).await.unwrap();
    assert_eq!(out["restarted"], true);
    // The reset itself announced the restart; the next exec is a plain
    // fresh start, not a surprise.
    let out = k
        .call(json!({"code": "print('z' in dir())"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["output"].as_str().unwrap().trim(), "False");
    assert!(out.get("restarted").is_none());
}

#[tokio::test]
async fn kernel_crash_is_detected_and_reported() {
    require_python!();
    let k = PyKernelTool::new();
    k.call(json!({"code": "import os"}), &ctx()).await.unwrap();
    // os._exit skips the driver loop entirely — the process just dies.
    // Same-turn shape may surface as the timeout-recovery path (stdout
    // EOF) or a broken-pipe error; the invariant is the NEXT call.
    let _ = k.call(json!({"code": "os._exit(3)"}), &ctx()).await;
    let out = k.call(json!({"code": "print(1)"}), &ctx()).await.unwrap();
    assert_eq!(out["restarted"], true);
    assert_eq!(out["output"].as_str().unwrap().trim(), "1");
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_kernel_tool_kills_the_kernel() {
    require_python!();
    // The kernel reports its own pid, so the liveness check is exact —
    // no pattern matching that could collide with sibling tests' kernels.
    let dir = std::env::temp_dir().join(format!("orca-kernel-drop-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pid_file = dir.join("kernel.pid");
    {
        let k = PyKernelTool::new();
        let code = format!(
            "import os\nopen({:?}, 'w').write(str(os.getpid()))",
            pid_file.to_str().unwrap()
        );
        let out = k.call(json!({"code": code}), &ctx()).await.unwrap();
        assert_eq!(out["state"], "ok");
    } // dropped
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // Gone or zombie both count as dead; SIGKILL lands synchronously in
    // Drop, but reaping is up to tokio's orphan queue.
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        stat.is_empty() || stat.starts_with('Z'),
        "kernel process must die with the tool (stat: {stat})"
    );
}

#[tokio::test]
async fn kernel_stats_track_liveness() {
    require_python!();
    let stats = orca_harness_tools::BackgroundStats::new();
    {
        let k = PyKernelTool::new().stats(stats.clone());
        assert_eq!(stats.kernels(), 0);
        k.call(json!({"code": "a = 1"}), &ctx()).await.unwrap();
        assert_eq!(stats.kernels(), 1);
        k.call(json!({"action": "reset"}), &ctx()).await.unwrap();
        assert_eq!(stats.kernels(), 0);
        k.call(json!({"code": "a = 1"}), &ctx()).await.unwrap();
        assert_eq!(stats.kernels(), 1);
    } // drop
    assert_eq!(stats.kernels(), 0);
}
