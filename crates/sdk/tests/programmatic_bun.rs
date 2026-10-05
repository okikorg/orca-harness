//! Programmatic tool calling through a real Bun interpreter in a Docker
//! sandbox: catalog visibility, batches through the run's policy and
//! extensions, real error messages, a nested call outliving the Bun
//! timeout, cancellation, subagent children, and removal of the `tools`
//! global from a later plain execution.

use async_trait::async_trait;
use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_sdk::{
    sandbox::{DockerProvisioner, EnvironmentSpec, ExecRequest, Network, Provisioner},
    *,
};
use orca_harness_tools::ToolDispatch;
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
mod common;

/// Records every call id that reaches policy.
struct Observe(Arc<Mutex<Vec<String>>>);
#[async_trait]
impl Extension for Observe {
    fn name(&self) -> &str {
        "observe"
    }
    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        self.0.lock().unwrap().push(call.id.clone());
        Ok(ToolDecision::Continue)
    }
}

fn result(messages: &[Message], id: &str) -> ToolResult {
    messages
        .iter()
        .find_map(|m| match m {
            Message::Tool { results } => results.iter().find(|r| r.call_id == id).cloned(),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {id}"))
}

fn bun(id: &str, code: &str, timeout_ms: u64) -> ModelResponse {
    ModelResponse::ToolCalls {
        content: None,
        usage: None,
        calls: vec![call(
            id,
            "bun_repl",
            json!({"code": code, "timeoutMs": timeout_ms}),
        )],
    }
}

/// A tool that waits until its future is dropped, counting the drop.
fn waiting(name: &str, dropped: Arc<AtomicUsize>, started: Arc<tokio::sync::Notify>) -> FnTool {
    struct DropFlag(Arc<AtomicUsize>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    FnTool::new(name, "", json!({}), move |_, _| {
        let flag = DropFlag(dropped.clone());
        let started = started.clone();
        async move {
            let _flag = flag;
            started.notify_one();
            std::future::pending::<()>().await;
            Ok(json!(null))
        }
    })
}

#[tokio::test]
#[ignore = "requires the server-built orca-agent-sandbox:local Docker image"]
async fn sandbox_bun_calls_registered_tools_through_the_run() {
    let root = common::temp_dir("programmatic-bun");
    let id = format!(
        "orca-ptc-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let mut spec = EnvironmentSpec::new().network(Network::Disabled);
    spec.image = Some("orca-agent-sandbox:local".into());
    let sandbox = DockerProvisioner::new(spec).start().await.unwrap();
    let test_root = root.clone();
    let test_sandbox = sandbox.clone();
    let mut task = tokio::spawn(async move {
        run_all(test_root, id, test_sandbox).await;
    });
    let outcome = tokio::time::timeout(Duration::from_secs(120), &mut task).await;
    if outcome.is_err() {
        task.abort();
        let _ = task.await;
    }
    sandbox.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
    outcome.expect("timed out").unwrap();
}

async fn run_all(root: std::path::PathBuf, id: String, sandbox: Arc<dyn sandbox::Sandbox>) {
    let harness = Harness::builder()
        .workspace(&root)
        .state_dir(root.join("state"))
        .build()
        .unwrap();
    let environment = |suffix: &str| {
        SessionEnvironment::sandbox(format!("{id}-{suffix}"), sandbox.clone(), "/workspace")
            .unwrap()
    };

    // Catalog, batch, policy, ids, real errors.
    let model = Arc::new(ScriptedModel::new(vec![
        bun(
            "bun-parent",
            "const catalog = (await tools.list()).map(x => x.name);
             if (catalog.includes('hidden') || catalog.includes('bun_repl')) throw new Error('catalog leaked ' + catalog);
             const results = await tools.batch([{name:'lookup',arguments:{n:40}},{name:'lookup',arguments:{n:2}}]);
             console.log('rpc=' + results.reduce((n,r)=>n+r.output.n,0));
             const hidden = await tools.batch([{name:'hidden',arguments:{}}]);
             console.log('hidden=' + hidden[0].output.error);
             try { await tools.call('fails', {}); } catch (error) { console.log('fails=' + error.message); }
             try { await tools.batch([]); } catch (error) { console.log('empty=' + error.message); }",
            20000,
        ),
        ModelResponse::final_text("done"),
    ]));
    let target = sandbox.clone();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let agent = harness
        .agent(model)
        .tools(ToolPreset::Coding)
        .bun()
        .programmatic_tools(ProgrammaticTools::new().visibility(Arc::new(|s| s.name != "hidden")))
        .extension(Observe(observed.clone()))
        .tool(FnTool::new("lookup", "", json!({}), move |input, ctx| {
            let target = target.clone();
            async move {
                assert!(ctx.call_id.starts_with("bun-parent.ptc"), "{}", ctx.call_id);
                let output = target
                    .exec(ExecRequest::new(
                        "printf sandbox-lookup > /workspace/ptc-proof",
                    ))
                    .await
                    .map_err(|_| ToolError::msg("write failed"))?;
                assert_eq!(output.exit_code, 0);
                Ok(json!({"n": input["n"]}))
            }
        }))
        .tool(FnTool::new("fails", "", json!({}), |_, _| async {
            Err(ToolError::msg("lookup backend refused"))
        }))
        .tool(FnTool::new("hidden", "", json!({}), |_, _| async {
            panic!("hidden executed");
            #[allow(unreachable_code)]
            Ok(json!(null))
        }))
        .build()
        .unwrap();
    let session = agent
        .new_session()
        .environment(environment("main"))
        .open()
        .unwrap();
    session.run("run").await.unwrap();
    let parent = result(&session.messages().await, "bun-parent");
    assert!(!parent.is_error, "{parent:?}");
    assert_eq!(parent.output["state"], "ok", "{parent:?}");
    let output = parent.output["output"].as_str().unwrap();
    assert!(output.contains("rpc=42"), "{output}");
    assert!(output.contains("hidden=unknown tool: hidden"), "{output}");
    assert!(output.contains("fails=lookup backend refused"), "{output}");
    assert!(
        output.contains("empty=a tools batch holds 1 to 64 calls, not 0"),
        "{output}"
    );
    assert_eq!(
        sandbox.read_file("/workspace/ptc-proof").await.unwrap(),
        b"sandbox-lookup"
    );
    assert!(!root.join("ptc-proof").exists());
    let nested: Vec<String> = observed
        .lock()
        .unwrap()
        .iter()
        .filter(|id| id.starts_with("bun-parent."))
        .cloned()
        .collect();
    assert_eq!(
        nested,
        ["bun-parent.ptc1", "bun-parent.ptc2", "bun-parent.ptc4"]
    );
    session.shutdown(Duration::from_secs(10)).await.unwrap();

    timeout_reports_a_timeout(&harness, environment("timeout")).await;
    cancellation_leaves_a_clean_interpreter(&harness, environment("cancel"), &sandbox).await;
    children_get_their_own_programmatic_bun(&harness, environment("child")).await;
    plain_execution_removes_the_global(&sandbox).await;
}

async fn timeout_reports_a_timeout(harness: &Harness, environment: SessionEnvironment) {
    let dropped = Arc::new(AtomicUsize::new(0));
    let model = Arc::new(ScriptedModel::new(vec![
        bun(
            "slow-bun",
            "globalThis.dirty = true; await tools.call('wait_client', {});",
            1500,
        ),
        bun(
            "after-timeout",
            "console.log('dirty=' + typeof globalThis.dirty);",
            20000,
        ),
        ModelResponse::final_text("done"),
    ]));
    let agent = harness
        .agent(model)
        .tools(ToolPreset::Coding)
        .bun()
        .programmatic_tools(ProgrammaticTools::new())
        .tool(waiting("wait_client", dropped.clone(), Arc::default()))
        .build()
        .unwrap();
    let session = agent.new_session().environment(environment).open().unwrap();
    session.run("slow").await.unwrap();
    let messages = session.messages().await;
    let slow = result(&messages, "slow-bun");
    assert!(!slow.is_error, "{slow:?}");
    assert_eq!(slow.output["state"], "timeout", "{slow:?}");
    assert!(
        slow.output["error"]
            .as_str()
            .unwrap()
            .contains("execution exceeded 1500ms"),
        "{slow:?}"
    );
    let after = result(&messages, "after-timeout");
    assert_eq!(after.output["restarted"], true, "{after:?}");
    assert_eq!(after.output["output"], "dirty=undefined", "{after:?}");
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    session.shutdown(Duration::from_secs(10)).await.unwrap();
}

async fn cancellation_leaves_a_clean_interpreter(
    harness: &Harness,
    environment: SessionEnvironment,
    sandbox: &Arc<dyn sandbox::Sandbox>,
) {
    let started = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(ScriptedModel::new(vec![
        bun(
            "cancel-bun",
            "globalThis.cancelDirty = true; setTimeout(() => Bun.write('/workspace/late-rpc-cancel','bad'), 500); await tools.call('wait_client', {});",
            20000,
        ),
        bun(
            "after-cancel",
            "if (globalThis.cancelDirty) throw new Error('cancelled interpreter reused'); console.log('cancel-clean');",
            20000,
        ),
        ModelResponse::final_text("clean"),
    ]));
    let agent = harness
        .agent(model)
        .tools(ToolPreset::Coding)
        .bun()
        .programmatic_tools(ProgrammaticTools::new())
        .tool(waiting("wait_client", Arc::default(), started.clone()))
        .build()
        .unwrap();
    let session = agent.new_session().environment(environment).open().unwrap();
    let token = CancellationToken::new();
    let trigger = token.clone();
    tokio::spawn(async move {
        started.notified().await;
        trigger.cancel();
    });
    assert!(session
        .run(RunRequest::new("cancel").cancellation(token))
        .await
        .is_err());
    session.run("after cancel").await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(sandbox
        .read_file("/workspace/late-rpc-cancel")
        .await
        .is_err());
    assert!(format!("{:?}", session.messages().await).contains("cancel-clean"));
    session.shutdown(Duration::from_secs(10)).await.unwrap();
}

async fn children_get_their_own_programmatic_bun(
    harness: &Harness,
    environment: SessionEnvironment,
) {
    let model = Arc::new(ScriptedModel::new(vec![
        bun(
            "child-bun",
            "console.log('child=' + await tools.call('child_lookup', {}));
             const nested = await tools.batch([{name:'bun_repl',arguments:{action:'reset'}}]);
             console.log('reentry=' + nested[0].output.error);",
            20000,
        ),
        ModelResponse::final_text("child done"),
    ]));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let agent = harness
        .agent(model.clone())
        .tools(ToolPreset::Coding)
        .programmatic_tools(ProgrammaticTools::new())
        .extension(Observe(observed.clone()))
        .subagents(SubagentConfig::default())
        .tool(FnTool::new(
            "child_lookup",
            "",
            json!({}),
            |_, ctx| async move {
                assert!(ctx.call_id.starts_with("child-bun.ptc"), "{}", ctx.call_id);
                Ok(json!(42))
            },
        ))
        .build()
        .unwrap();
    let session = agent.new_session().environment(environment).open().unwrap();
    session
        .subagents()
        .unwrap()
        .run(
            orca_harness_sdk::orchestration::SubagentRequest::new("programmatic child"),
            None,
            None,
        )
        .await
        .unwrap();
    let text = format!("{:?}", model.observed_contexts().last().unwrap());
    assert!(text.contains("child=42"), "{text}");
    assert!(text.contains("reentry=unknown tool: bun_repl"), "{text}");
    assert!(observed
        .lock()
        .unwrap()
        .iter()
        .any(|id| id == "child-bun.ptc1"));
    session.shutdown(Duration::from_secs(10)).await.unwrap();
}

async fn plain_execution_removes_the_global(sandbox: &Arc<dyn sandbox::Sandbox>) {
    let ctx = ToolContext {
        call_id: "direct".into(),
        tool_name: "bun_repl".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    };
    let repl = Arc::new(
        BunReplTool::new()
            .sandbox(sandbox.clone())
            .working_dir("/workspace"),
    );
    let dispatching =
        repl.clone()
            .with_dispatch(ToolDispatch::new([], [], ProgrammaticTools::new()));
    let code = json!({"code": "console.log(typeof globalThis.tools)"});
    let installed: Value = dispatching.call(code.clone(), &ctx).await.unwrap();
    assert_eq!(installed["output"], "object", "{installed:?}");
    let plain = repl.call(code, &ctx).await.unwrap();
    assert_eq!(plain["output"], "undefined", "{plain:?}");
    repl.reset().await.unwrap();
}
