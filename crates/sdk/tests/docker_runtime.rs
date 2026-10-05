//! Actual interpreter and child execution in the server-built local image.
use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_sdk::orchestration::SubagentRequest;
use orca_harness_sdk::sandbox::{
    DockerProvisioner, EnvironmentSpec, ExecRequest, Network, Provisioner, Sandbox,
};
use orca_harness_sdk::{
    Harness, Message, ModelResponse, SessionEnvironment, SubagentConfig, ToolPreset,
};
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
mod common;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(std::io::Error::other(message).into())
    }
}
fn round(python_id: &str, python: &str, bun_id: &str, bun: &str) -> ModelResponse {
    ModelResponse::ToolCalls {
        content: None,
        usage: None,
        calls: vec![
            call(
                python_id,
                "pykernel",
                json!({"code":python,"timeoutMs":10000}),
            ),
            call(bun_id, "bun_repl", json!({"code":bun,"timeoutMs":10000})),
        ],
    }
}
fn output(messages: &[Message], id: &str) -> Result<Value> {
    for message in messages {
        if let Message::Tool { results } = message {
            for result in results {
                if result.call_id == id {
                    require(
                        !result.is_error && result.output.get("error").is_none_or(Value::is_null),
                        &format!("tool {id} failed: {}", result.output),
                    )?;
                    return Ok(result.output.clone());
                }
            }
        }
    }
    Err(std::io::Error::other(format!("missing tool result {id}")).into())
}
async fn proof(root: &std::path::Path, sandbox: Arc<dyn Sandbox>, identity: &str) -> Result<()> {
    let model=Arc::new(ScriptedModel::new(vec![
        round("p1","import os\ncounter = 40\nopen('/workspace/python-proof','w').write('python-proof')\nprint('python_uid=' + str(os.getuid()))", "b1", "globalThis.counter = 40; await Bun.write('/workspace/bun-proof', 'bun-proof'); console.log('bun_uid=' + process.getuid());"),
        ModelResponse::final_text("initialized"),
        round("p2","counter += 2\nprint(counter)","b2","globalThis.counter += 2; console.log(globalThis.counter);"),
        ModelResponse::final_text("persisted"),
        round("p3","assert 'counter' not in globals()\nprint('python-reset-ok')","b3","if ('counter' in globalThis) throw new Error('Bun state survived reset'); console.log('bun-reset-ok');"),
        ModelResponse::final_text("reset"),
        ModelResponse::ToolCalls {content:None,usage:None,calls:vec![call("child-shell","shell",json!({"command":"cat /workspace/python-proof /workspace/bun-proof && printf inherited > /workspace/child-proof && id -u"}))]},
        ModelResponse::final_text("child completed"),
    ]));
    let harness = Harness::builder()
        .workspace(root)
        .state_dir(root.join("state"))
        .build()?;
    let agent = harness
        .agent(model.clone())
        .tools(ToolPreset::Coding)
        .python()
        .bun()
        .subagents(SubagentConfig::default())
        .build()?;
    let session = agent
        .new_session()
        .persistent()
        .environment(SessionEnvironment::sandbox(
            identity,
            sandbox.clone(),
            "/workspace",
        )?)
        .open()?;
    session.run("initialize both runtimes").await?;
    let first = session.messages().await;
    require(
        output(&first, "p1")?
            .to_string()
            .contains("python_uid=65532"),
        "Python ran with incorrect identity",
    )?;
    require(
        output(&first, "b1")?.to_string().contains("bun_uid=65532"),
        "Bun ran with incorrect identity",
    )?;
    session.run("read persistent state").await?;
    let second = session.messages().await;
    require(
        output(&second, "p2")?.to_string().contains("42"),
        "Python state was lost between turns",
    )?;
    require(
        output(&second, "b2")?.to_string().contains("42"),
        &format!(
            "Bun state was lost between turns: first={}, second={}",
            output(&first, "b1")?,
            output(&second, "b2")?
        ),
    )?;
    session.reset_in_place().await?;
    session.run("verify reset").await?;
    let third = session.messages().await;
    require(
        output(&third, "p3")?
            .to_string()
            .contains("python-reset-ok"),
        "Python reset did not clear state",
    )?;
    require(
        output(&third, "b3")?.to_string().contains("bun-reset-ok"),
        "Bun reset did not clear state",
    )?;
    session
        .subagents()
        .unwrap()
        .run(SubagentRequest::new("prove inherited sandbox"), None, None)
        .await?;
    let contexts = model.observed_contexts();
    require(
        output(contexts.last().unwrap().messages(), "child-shell")?
            .to_string()
            .contains("65532"),
        "child command did not use sandbox identity",
    )?;
    require(
        sandbox.read_file("/workspace/python-proof").await? == b"python-proof",
        "Python remote file missing",
    )?;
    require(
        sandbox.read_file("/workspace/bun-proof").await? == b"bun-proof",
        "Bun remote file missing",
    )?;
    require(
        sandbox.read_file("/workspace/child-proof").await? == b"inherited",
        "child sandbox file missing",
    )?;
    require(
        !root.join("python-proof").exists()
            && !root.join("bun-proof").exists()
            && !root.join("child-proof").exists(),
        "execution wrote host workspace",
    )?;
    session.shutdown(Duration::from_secs(10)).await?;
    require(
        sandbox.exec(ExecRequest::new("printf alive")).await?.stdout == b"alive",
        "SDK shutdown destroyed host-owned sandbox",
    )?;
    Ok(())
}
#[tokio::test]
#[ignore = "build sandbox/Dockerfile as orca-agent-sandbox:local before explicitly running"]
async fn real_python_bun_persistence_reset_and_subagent_sandbox_inheritance() {
    let root = common::temp_dir("docker-runtimes");
    let identity = format!(
        "orca-runtime-test-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let mut spec = EnvironmentSpec::new().network(Network::Disabled);
    spec.image = Some(
        std::env::var("ORCA_TEST_RUNTIME_IMAGE")
            .unwrap_or_else(|_| "orca-agent-sandbox:local".into()),
    );
    let provisioner = DockerProvisioner::new(spec).named(&identity, &identity);
    let initial = provisioner.start().await.unwrap();
    let result = async {
        let sandbox =
            DockerProvisioner::finalize_named(&identity, &identity, "/workspace", &[]).await?;
        proof(&root, sandbox, &identity).await
    };
    let result = tokio::time::timeout(Duration::from_secs(90), result).await;
    let cleanup = initial.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
    cleanup.unwrap();
    result.unwrap().unwrap();
}
