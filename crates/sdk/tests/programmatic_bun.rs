use async_trait::async_trait;
use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_sdk::{
    sandbox::{DockerProvisioner, EnvironmentSpec, ExecRequest, Network, Provisioner},
    *,
};
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
mod common;
struct Observe(Arc<Mutex<Vec<ToolInvocation>>>);
#[async_trait]
impl Extension for Observe {
    fn name(&self) -> &str {
        "observe"
    }
    async fn before_tool(&self, _: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        self.0
            .lock()
            .unwrap()
            .push(current_tool_invocation().unwrap());
        Ok(ToolDecision::Continue)
    }
}
#[tokio::test]
#[ignore = "requires server-built orca-agent-sandbox:local Docker image"]
async fn sandbox_bun_dispatches_registered_tools_with_run_policy_and_disables_on_next_run() {
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
    let initial = DockerProvisioner::new(spec)
        .named(&id, &id)
        .start()
        .await
        .unwrap();
    let test_root = root.clone();
    let test_id = id.clone();
    let mut task = tokio::spawn(async move {
        let root = test_root;
        let id = test_id;
        let sandbox = DockerProvisioner::finalize_named(&id, &id, "/workspace", &[])
            .await
            .unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            ModelResponse::ToolCalls {
                content: None,
                usage: None,
                calls: vec![call(
                    "bun-parent",
                    "bun_repl",
                    json!({"code":"const catalog = await tools.list(); if (catalog.some(x => x.name === 'hidden')) throw new Error('hidden leaked'); const results = await tools.batch([{name:'lookup',arguments:{n:40}},{name:'lookup',arguments:{n:2}}]); console.log('rpc=' + results.reduce((n,r)=>n+r.output.n,0)); console.log('uid=' + process.getuid());","timeoutMs":20000}),
                )],
            },
            ModelResponse::final_text("done"),
            ModelResponse::ToolCalls {
                content: None,
                usage: None,
                calls: vec![call(
                    "disabled",
                    "bun_repl",
                    json!({"code":"if (globalThis.tools !== undefined) throw new Error('RPC survived run'); console.log('disabled-ok');"}),
                )],
            },
            ModelResponse::final_text("disabled"),
        ]));
        let harness = Harness::builder()
            .workspace(&root)
            .state_dir(root.join("state"))
            .build()
            .unwrap();
        let target = sandbox.clone();
        let agent = harness
            .agent(model)
            .tools(ToolPreset::Coding)
            .bun()
            .tool(FnTool::new(
                "lookup",
                "lookup",
                json!({}),
                move |input, ctx| {
                    let target = target.clone();
                    async move {
                        assert_eq!(ctx.parent_call_id().as_deref(), Some("bun-parent"));
                        let output = target
                            .exec(ExecRequest::new(
                                "printf sandbox-lookup > /workspace/ptc-proof",
                            ))
                            .await
                            .map_err(|_| ToolError::msg("write failed"))?;
                        assert_eq!(output.exit_code, 0);
                        Ok(json!({"n":input["n"]}))
                    }
                },
            ))
            .tool(FnTool::new("hidden", "hidden", json!({}), |_, _| async {
                panic!("hidden executed");
                #[allow(unreachable_code)]
                Ok(json!(null))
            }))
            .build()
            .unwrap();
        let session = agent
            .new_session()
            .environment(SessionEnvironment::sandbox(&id, sandbox.clone(), "/workspace").unwrap())
            .open()
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        session
            .run(
                RunRequest::new("run")
                    .extension(Observe(events.clone()))
                    .programmatic_tools(
                        ProgrammaticTools::new().visibility(Arc::new(|s| s.name != "hidden")),
                    ),
            )
            .await
            .unwrap();
        let messages = session.messages().await;
        let result = messages
            .iter()
            .find_map(|m| {
                if let Message::Tool { results } = m {
                    results.iter().find(|r| r.call_id == "bun-parent")
                } else {
                    None
                }
            })
            .unwrap();
        assert!(!result.is_error, "{:?}", result);
        assert_eq!(result.output["state"], "ok", "{:?}", result);
        assert!(result.output.to_string().contains("rpc=42"), "{:?}", result);
        assert_eq!(
            sandbox.read_file("/workspace/ptc-proof").await.unwrap(),
            b"sandbox-lookup"
        );
        assert!(!root.join("ptc-proof").exists());
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e.parent_call_id.as_deref() == Some("bun-parent"))
                .count(),
            2
        );
        session.run("disabled").await.unwrap();
        let messages = session.messages().await;
        assert!(format!("{messages:?}").contains("disabled-ok"));
        session.shutdown(Duration::from_secs(10)).await.unwrap();
        let child_model = Arc::new(ScriptedModel::new(vec![
            ModelResponse::ToolCalls {
                content: None,
                usage: None,
                calls: vec![call(
                    "child-bun",
                    "bun_repl",
                    json!({"code":"console.log('child=' + await tools.call('child_lookup', {})); const denied = await tools.batch([{name:'bun_repl',arguments:{action:'reset'}}]); if (!denied[0].is_error) throw new Error('recursive reset executed');"}),
                )],
            },
            ModelResponse::final_text("child done"),
        ]));
        let child_observed = Arc::new(Mutex::new(Vec::new()));
        let child_agent = harness
            .agent(child_model.clone())
            .tools(ToolPreset::Coding)
            .programmatic_tools(ProgrammaticTools::new())
            .extension(Observe(child_observed.clone()))
            .subagents(SubagentConfig::default())
            .tool(FnTool::new(
                "child_lookup",
                "",
                json!({}),
                |_, ctx| async move {
                    assert_eq!(ctx.parent_call_id().as_deref(), Some("child-bun"));
                    Ok(json!(42))
                },
            ))
            .build()
            .unwrap();
        let child_session = child_agent
            .new_session()
            .environment(
                SessionEnvironment::sandbox(format!("{id}-child"), sandbox.clone(), "/workspace")
                    .unwrap(),
            )
            .open()
            .unwrap();
        child_session
            .subagents()
            .unwrap()
            .run(
                orca_harness_sdk::orchestration::SubagentRequest::new("programmatic child"),
                None,
                None,
            )
            .await
            .unwrap();
        let contexts = child_model.observed_contexts();
        let text = format!("{:?}", contexts.last().unwrap());
        assert!(text.contains("child=42"), "{text}");
        assert!(!text.contains("execution exceeded"), "{text}");
        assert!(child_observed
            .lock()
            .unwrap()
            .iter()
            .any(|i| i.parent_call_id.as_deref() == Some("child-bun")));
        child_session
            .shutdown(Duration::from_secs(10))
            .await
            .unwrap();
        let waiting = Arc::new(tokio::sync::Notify::new());
        let notify = waiting.clone();
        let cancel_model = Arc::new(ScriptedModel::new(vec![
            ModelResponse::ToolCalls {
                content: None,
                usage: None,
                calls: vec![call(
                    "cancel-bun",
                    "bun_repl",
                    json!({"code":"globalThis.cancelDirty = true; setTimeout(() => Bun.write('/workspace/late-rpc-cancel','bad'), 500); await tools.call('wait_client', {});"}),
                )],
            },
            ModelResponse::ToolCalls {
                content: None,
                usage: None,
                calls: vec![call(
                    "after-cancel",
                    "bun_repl",
                    json!({"code":"if (globalThis.cancelDirty) throw new Error('cancelled interpreter reused'); console.log('cancel-clean');"}),
                )],
            },
            ModelResponse::final_text("clean"),
        ]));
        let cancel_agent = harness
            .agent(cancel_model)
            .tools(ToolPreset::Coding)
            .bun()
            .programmatic_tools(ProgrammaticTools::new())
            .tool(FnTool::new("wait_client", "", json!({}), move |_, _| {
                let notify = notify.clone();
                async move {
                    notify.notify_one();
                    std::future::pending::<()>().await;
                    Ok(json!(null))
                }
            }))
            .build()
            .unwrap();
        let cancel_session = cancel_agent
            .new_session()
            .environment(
                SessionEnvironment::sandbox(format!("{id}-cancel"), sandbox.clone(), "/workspace")
                    .unwrap(),
            )
            .open()
            .unwrap();
        let token = CancellationToken::new();
        let trigger = token.clone();
        tokio::spawn(async move {
            waiting.notified().await;
            trigger.cancel();
        });
        assert!(cancel_session
            .run(RunRequest::new("cancel").cancellation(token))
            .await
            .is_err());
        cancel_session.run("after cancel").await.unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(sandbox
            .read_file("/workspace/late-rpc-cancel")
            .await
            .is_err());
        assert!(format!("{:?}", cancel_session.messages().await).contains("cancel-clean"));
        cancel_session
            .shutdown(Duration::from_secs(10))
            .await
            .unwrap();
    });
    let result = tokio::time::timeout(Duration::from_secs(60), &mut task).await;
    if result.is_err() {
        task.abort();
        let _ = task.await;
    }
    initial.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
    result.unwrap().unwrap();
}
