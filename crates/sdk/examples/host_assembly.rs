//! Offline, asserted host assembly: prepare, review, and archive a release checklist.
//!
//! Exercises SDK state, tools/policy, memory/skills, real local stdio MCP, sessions,
//! streamed run events, recovery, and session-owned background orchestration.
//! Models are deterministic test doubles, not providers. This is NOT coverage of
//! every provider/integration: no credentials, HTTP MCP, web, remote skills,
//! containers, or language kernels. The OS process demo requires a POSIX shell.
//! The same executable has a private MCP-server mode; no Python/Node is needed.

mod support;

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_sdk::integrations::mcp::StdioLaunch;
use orca_harness_sdk::orchestration::{ProcessSpawn, RunState, SubagentModel, SubagentRequest};
use orca_harness_sdk::*;
use serde_json::{json, Value};
use support::TempWorkspace;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
const WAIT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> Result {
    if std::env::args().nth(1).as_deref() == Some("--release-mcp") {
        return serve_mcp();
    }
    let workspace = TempWorkspace::new("host-assembly");
    let harness = Harness::builder()
        .workspace(workspace.path())
        .state_dir(workspace.path().join(".orca"))
        .build()?;
    assert_eq!(harness.workspace().root(), workspace.path().canonicalize()?);
    println!(
        "Release workspace: {}",
        harness.workspace().root().display()
    );
    let mcp = harness.mcp();
    let status = mcp
        .connect_stdio(
            "release",
            &StdioLaunch {
                command: std::env::current_exe()?.to_string_lossy().into_owned(),
                args: vec!["--release-mcp".into()],
                env: Default::default(),
                cwd: None,
                environment: Default::default(),
            },
        )
        .await?;
    assert!(status.healthy);
    assert_eq!(status.tool_count, 1);

    // Always disconnect, even when a fallible phase returns an error. Sessions
    // likewise shut down before propagating phase errors; workspace is RAII-owned.
    let result = async {
        let id = prepare(&harness, mcp.clone()).await?;
        recover_and_archive(&harness, &id).await?;
        interrupted_checks(&harness).await?;
        review(&harness).await
    }
    .await;
    assert!(mcp.disconnect("release"));
    assert!(mcp.servers().is_empty());
    result?;
    println!("All host assembly assertions passed; sessions closed and MCP disconnected.");
    Ok(())
}

/// A minimal JSON-lines MCP peer: preserve request IDs and implement a real
/// tools/call, rather than shadowing a remote tool with a host FnTool.
fn serve_mcp() -> Result {
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line?)?;
        let Some(id) = request.get("id") else {
            continue;
        };
        let result = match request["method"].as_str().unwrap_or("") {
            "initialize" => {
                json!({"protocolVersion":"2025-06-18", "capabilities":{"tools":{}},
                "serverInfo":{"name":"release-fixture","version":"1"}})
            }
            "tools/list" => {
                json!({"tools":[{"name":"check", "description":"Check a release tag",
                "inputSchema":{"type":"object","properties":{"tag":{"type":"string"}},"required":["tag"]}}]})
            }
            "tools/call" => {
                assert_eq!(request["params"]["name"], "check");
                let tag = request["params"]["arguments"]["tag"].as_str().unwrap();
                json!({
                    "content": [{
                        "type": "text",
                        "text": format!("verified tag {tag}"),
                    }],
                    "isError": false,
                })
            }
            _ => {
                writeln!(
                    out,
                    "{}",
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"unsupported method"}})
                )?;
                out.flush()?;
                continue;
            }
        };
        writeln!(out, "{}", json!({"jsonrpc":"2.0","id":id,"result":result}))?;
        out.flush()?;
    }
    Ok(())
}

fn step(calls: Vec<ToolCall>) -> ModelResponse {
    ModelResponse::ToolCalls {
        content: None,
        calls,
        usage: Some(Usage {
            input_tokens: 10,
            output_tokens: 3,
            ..Default::default()
        }),
    }
}

fn tool_output(messages: &[Message], id: &str) -> Value {
    messages
        .iter()
        .find_map(|m| match m {
            Message::Tool { results } => results.iter().find(|r| r.call_id == id).map(|r| {
                assert!(!r.is_error, "{r:?}");
                r.output.clone()
            }),
            _ => None,
        })
        .expect("tool result must be in transcript")
}

async fn prepare(harness: &Harness, mcp: Mcp) -> Result<String> {
    let memory = harness.memory()?;
    let preference = memory.save(
        "Rust release checklist must stay concise",
        "preference",
        false,
        "host",
    )?;
    let draft = memory.save("draft release", "workflow", false, "host")?;
    assert_eq!(
        memory
            .update(&draft.id, "review release", None)?
            .unwrap()
            .content,
        "review release"
    );
    assert!(memory.forget(&draft.id)?);
    assert_eq!(memory.list(10)?.len(), 1);
    assert_eq!(memory.search("Rust", 5)?[0].id, preference.id);

    // Explicit roots exclude the user's real home skills.
    let skills = Skills::new(
        harness.workspace().root(),
        Some(harness.state_dir().into()),
        None,
    );
    let path = skills.scaffold("release_checklist", SkillDestination::Workspace)?;
    std::fs::write(
        path,
        "---\nname: release_checklist\ndescription: Review a Rust release\n---\nRun tests before tagging.\n",
    )?;
    assert_eq!(skills.reload().skills.len(), 1);
    skills.disable("release_checklist");
    assert!(skills.tool().is_none());
    skills.enable("release_checklist");

    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let collect = FnTool::new(
        "collect_checks",
        "Collect release evidence (idempotent)",
        json!({"type":"object"}),
        move |_, _| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err(ToolError::msg("temporary evidence-store failure"))
                } else {
                    Ok(json!({"evidence":"tests passed; ".repeat(80)}))
                }
            }
        },
    );
    let forbidden_calls = Arc::new(AtomicUsize::new(0));
    let counter = forbidden_calls.clone();
    let publish = FnTool::new(
        "publish_release",
        "Publish a release",
        json!({"type":"object"}),
        move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            async { Ok(json!({"published":true})) }
        },
    );
    let model = Arc::new(ScriptedModel::new(vec![
        step(vec![
            call("skill", "skill", json!({"name":"release_checklist"})),
            call("evidence", "collect_checks", json!({})),
            call("denied", "publish_release", json!({})),
            call(
                "select",
                "mcp_select_tool",
                json!({"name":"mcp__release__check"}),
            ),
        ]),
        step(vec![call(
            "tag",
            "mcp__release__check",
            json!({"tag":"v1.0"}),
        )]),
        ModelResponse::final_text("Release checklist prepared; publication requires approval."),
    ]));
    let agent = harness
        .agent(model.clone())
        .name("release-host")
        .system_prompt("Prepare a concise Rust release checklist.")
        .tool(collect)
        .tool(publish)
        .policy(ToolPolicy::new().rule(|c: &ToolCall| {
            if c.name == "publish_release" {
                PolicyOutcome::Deny("host approval required".into())
            } else {
                PolicyOutcome::Allow
            }
        }))
        .tool_retry(RetryConfig::attempts(2).backoff_ms(0))
        .truncation(TruncationConfig {
            max_string_chars: 128,
            ..Default::default()
        })
        .memory(MemoryConfig::read_write(memory))
        .skills(skills)
        .mcp(mcp)
        .events(true)
        .usage(true)
        .build()?;
    let session = agent.new_session().persistent().open()?;
    let id = session.id().unwrap();
    let result = async {
        let mut handle = session
            .start(RunRequest::new("Prepare the Rust release checklist").event_capacity(128))?;
        let mut events = handle.take_events().unwrap();
        let reader = tokio::spawn(async move {
            let mut count = 0;
            while let Some(event) = events.recv().await {
                match event {
                    RunEvent::Harness(_) => count += 1,
                    RunEvent::Overflow { .. } => panic!("unexpected overflow"),
                }
            }
            count
        });
        let outcome = tokio::time::timeout(WAIT, handle.outcome()).await??;
        assert!(reader.await? > 0);
        assert_eq!(outcome.dropped_events, 0);
        let result = outcome.into_result()?;
        // Only responses carrying Usage are metered.
        assert_eq!(result.metered_steps, 2);
        assert_eq!(result.usage.input_tokens, 20);
        assert_eq!(result.usage.output_tokens, 6);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(forbidden_calls.load(Ordering::SeqCst), 0);
        let transcript = serde_json::to_string(&result.messages)?;
        assert!(transcript.contains("host approval required"));
        assert!(tool_output(&result.messages, "skill")
            .to_string()
            .contains("Run tests before tagging"));
        assert!(tool_output(&result.messages, "tag")
            .to_string()
            .contains("verified tag v1.0"));
        assert!(transcript.contains("_readFull"));
        assert!(model
            .observed_contexts()
            .iter()
            .any(|ctx| serde_json::to_string(ctx.messages())
                .unwrap()
                .contains(&preference.content)));
        assert!(harness.sessions().list().iter().any(|s| s.meta.id == id));
        println!("Prepared: policy denied publication, evidence retried, skill and MCP loaded.");
        Ok::<_, Box<dyn std::error::Error>>(id)
    }
    .await;
    session.shutdown(WAIT).await?;
    result
}

/// Resume on a new agent, recover the untruncated evidence, then compact a
/// separate branch. The original persistent conversation remains unchanged.
async fn recover_and_archive(harness: &Harness, id: &str) -> Result {
    let agent = harness
        .agent(ScriptedModel::tool_round(
            vec![call(
                "recover",
                "read_tool_result",
                json!({"callId":"evidence","maxChars":4096}),
            )],
            "Evidence recovered",
        ))
        .compaction(Compaction::Manual)
        .build()?;
    let session = agent.resume_session(id)?;
    let result = async {
        let result = session.run("Recover release evidence").await?;
        assert!(tool_output(&result.messages, "recover")
            .to_string()
            .contains(&"tests passed; ".repeat(80)));
        let before = session.messages().await;
        let fork = session.fork().await?;
        assert_ne!(fork.id(), session.id());
        let report = fork
            .compact(CompactConfig {
                tail_budget_tokens: 10,
            })
            .await?;
        assert!(report.messages_after < report.messages_before);
        assert!(!report.summary.is_empty());
        assert_eq!(
            serde_json::to_value(session.messages().await)?,
            serde_json::to_value(before)?
        );
        fork.shutdown(WAIT).await?;
        println!("Resumed evidence and compacted an isolated archive fork.");
        Ok(())
    }
    .await;
    session.shutdown(WAIT).await?;
    result
}

/// Inject one model transport error, then exercise partial model failure and
/// caller cancellation after a metered tool step (not just before starting).
struct RetryOnce {
    calls: AtomicUsize,
}
#[async_trait]
impl Model for RetryOnce {
    async fn generate(
        &self,
        _: &Context,
        _: &[ToolSchema],
    ) -> std::result::Result<ModelResponse, ModelError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(ModelError::Request("temporary transport error".into()))
        } else {
            Ok(ModelResponse::final_text("retry recovered"))
        }
    }
}

async fn interrupted_checks(harness: &Harness) -> Result {
    let model = Arc::new(RetryOnce {
        calls: AtomicUsize::new(0),
    });
    let agent = harness
        .agent(model.clone())
        .model_retry(RetryConfig::attempts(2).backoff_ms(0))
        .build()?;
    let session = agent.new_session().ephemeral().open()?;
    assert_eq!(
        session.run("retry release check").await?.text,
        "retry recovered"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    session.shutdown(WAIT).await?;
    for cancel in [false, true] {
        let token = CancellationToken::new();
        let stop = token.clone();
        let tool = FnTool::new(
            "check",
            "Checkpoint",
            json!({"type":"object"}),
            move |_, ctx| {
                let stop = stop.clone();
                async move {
                    if cancel {
                        stop.cancel();
                        ctx.cancellation.cancelled().await;
                    }
                    Ok(json!({"checkpoint":"release checked"}))
                }
            },
        );
        let agent = harness
            .agent(ScriptedModel::new(vec![step(vec![call(
                "check",
                "check",
                json!({}),
            )])]))
            .tool(tool)
            .build()?;
        let session = agent.new_session().ephemeral().open()?;
        let outcome = session
            .run_outcome(RunRequest::new("check release").cancellation(token))
            .await?;
        assert!(matches!(
            &outcome.execution,
            Err(SdkError::Harness(error))
                if if cancel {
                    matches!(error, HarnessError::Cancelled)
                } else {
                    matches!(error, HarnessError::Model(_))
                }
        ));
        assert_eq!(outcome.metered_steps, 1);
        assert_eq!(outcome.usage.input_tokens, 10);
        assert!(outcome.persistence.is_ok());
        if !cancel {
            assert_eq!(
                tool_output(&outcome.messages, "check")["checkpoint"],
                "release checked"
            );
        }
        session.shutdown(WAIT).await?;
    }
    println!("Model retry, partial failure, and cooperative cancellation asserted.");
    Ok(())
}

/// A dedicated child route echoes rendered tasks; a gate proves detached work
/// can outlive the parent turn without consuming the parent's scripted replies.
struct Reviewer {
    release: CancellationToken,
    calls: AtomicUsize,
}
#[async_trait]
impl Model for Reviewer {
    async fn generate(
        &self,
        ctx: &Context,
        _: &[ToolSchema],
    ) -> std::result::Result<ModelResponse, ModelError> {
        self.release.cancelled().await;
        self.calls.fetch_add(1, Ordering::SeqCst);
        let task = ctx
            .messages()
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .unwrap();
        Ok(ModelResponse::final_text(if task == "release dimensions" {
            r#"["tests","docs"]"#
        } else {
            task
        }))
    }
}
async fn wait_for(mut condition: impl FnMut() -> bool) -> Result {
    tokio::time::timeout(WAIT, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}
async fn review(harness: &Harness) -> Result {
    let release = CancellationToken::new();
    let child = Arc::new(Reviewer {
        release: release.clone(),
        calls: AtomicUsize::new(0),
    });
    let parent = Arc::new(ScriptedModel::new(vec![
        ModelResponse::final_text("Review delegated"),
        ModelResponse::final_text("Review reports incorporated"),
    ]));
    let agent = harness
        .agent(parent.clone())
        .tools(ToolPreset::Coding)
        .processes(ProcessConfig::new().max_processes(2))
        .subagents(
            SubagentConfig::new()
                .background_limit(4)
                .model(SubagentModel::new(
                    "local/reviewer",
                    "offline release reviewer",
                    child.clone(),
                )),
        )
        .build()?;
    let session = agent.new_session().persistent().open()?;
    let processes = session.processes().unwrap();
    let result = async {
        let subagents = session.subagents().unwrap();
        assert!(subagents.settings().set_default_model(Some("local/reviewer".into())));
        let mut notifications = session.notifications();
        let ack = subagents.spawn(SubagentRequest::new("audit release checklist"))?;
        assert_eq!(
            session.run("Delegate release review").await?.text,
            "Review delegated"
        );
        assert_eq!(subagents.active().len(), 1);
        release.cancel();
        tokio::time::timeout(WAIT, async {
            loop {
                if let BackgroundNotification::SubagentFinished(n) = notifications.recv().await? {
                    if n.spawn.id == ack.spawn_id {
                        assert_eq!(
                            n.result.unwrap()["answer"],
                            "audit release checklist"
                        );
                        break;
                    }
                }
            }
            Ok::<_, Box<dyn std::error::Error>>(())
        }).await??;
        wait_for(|| session.pending_completions() == 1).await?;

        let workflows = session.workflows().unwrap();
        let mut source = Stage::new("source", "release dimensions");
        source.schema = Some("string[]".into());
        let mut map = Stage::new("checks", "check {{ item }}");
        map.kind = Kind::Map;
        map.over = Some("source".into());
        let mut summary = Stage::new("summary", "review: {{ stages.checks.output }}");
        summary.needs = vec!["checks".into()];
        let workflow = workflows
            .submit(
                WorkflowSubmission::new([source, map, summary])
                    .max_stages(8)
                    .timeout(WAIT),
            )?;
        wait_for(|| {
            workflows
                .status(workflow.run_id)
                .is_some_and(|s| s.outcome.is_some())
                && session.pending_completions() == 2
        })
        .await?;
        let status = workflows.status(workflow.run_id).unwrap();
        assert_eq!(status.state, RunState::Done);
        let outcome = status.outcome.unwrap();
        assert_eq!(outcome.outputs["summary"], r#"review: ["check tests","check docs"]"#);
        assert_eq!(
            workflows.stage_output(workflow.run_id, "summary")?.answer,
            outcome.outputs["summary"]
        );
        assert_eq!(child.calls.load(Ordering::SeqCst), 5);
        assert_eq!(
            session.continue_run(RunRequest::continuation()).await?.text,
            "Review reports incorporated"
        );
        assert_eq!(session.pending_completions(), 0);
        let transcript = serde_json::to_string(&session.messages().await)?;
        assert!(transcript.contains("background_subagent_completions"));
        assert!(transcript.contains("audit release checklist"));
        assert!(transcript.contains("check tests"));
        tokio::time::timeout(WAIT, async {
            loop {
                if let BackgroundNotification::CompletionsDelivered { spawn_ids } =
                    notifications.recv().await?
                {
                    assert_eq!(spawn_ids.len(), 2);
                    break;
                }
            }
            Ok::<_, Box<dyn std::error::Error>>(())
        }).await??;
        assert_eq!(parent.observed_contexts().len(), 2);

        let process = processes
            .spawn(
                ProcessSpawn::new(
                    "read start; printf 'release-ready\\n'; read finish; printf 'release-done\\n'",
                )
                .notify_on_match("release-ready"),
                None,
            )
            .await?;
        // Register first, then let the process emit readiness: no startup race.
        processes.write(&process.id, orca_harness_sdk::orchestration::ProcessWrite::new("start")).await?;
        // Readiness is explicitly synchronized via stdin rather than sleeps.
        let mut ready = false;
        tokio::time::timeout(WAIT, async {
            loop {
                if let BackgroundNotification::ProcessNotified(n) = notifications.recv().await? {
                    if n.id != process.id { continue; }
                    match n.kind {
                        ProcessNotificationKind::OutputMatch { pattern } => {
                            assert_eq!(pattern, "release-ready"); ready = true;
                            processes
                                .write(
                                    &process.id,
                                    orca_harness_sdk::orchestration::ProcessWrite::new("finish"),
                                )
                                .await?;
                        }
                        ProcessNotificationKind::Exit { exit_code } => {
                            assert!(ready);
                            assert_eq!(exit_code, Some(0));
                            break;
                        }
                    }
                }
            }
            Ok::<_, Box<dyn std::error::Error>>(())
        }).await??;
        println!("Dedicated review route, dependency/map outputs, completion delivery, OS readiness/exit asserted.");
        Ok(())
    }.await;
    session.shutdown(WAIT).await?;
    assert!(!processes.is_open());
    assert!(session.run("after shutdown").await.is_err());
    result
}
