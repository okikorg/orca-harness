//! End-to-end release-review host assembled from the SDK's production adapters.
//!
//! The example exercises provider authentication, persistent sessions, memory,
//! skills, MCP, constrained processes, detached subagents, workflows, event
//! streaming, continuation turns, and shutdown. Model calls are live rather than
//! scripted, so provider credentials and a POSIX shell are required. See the SDK
//! README for invocation details, expected cost, and coverage limits.
mod support;

use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use orca_harness_sdk::integrations::mcp::StdioLaunch;
use orca_harness_sdk::orchestration::{RunState, SubagentModel};
use orca_harness_sdk::{
    AnthropicModel, BackgroundNotification, BearerCredential, CodexCredential,
    CodexCredentialSource, CredentialError, CredentialSource, Harness, Limits, MemoryConfig,
    Message, Model, OpenAiCodexModel, OpenAiModel, OpenRouterModel, PolicyOutcome, ProcessConfig,
    RetryConfig, RunRequest, Session, SkillDestination, Skills, SubagentConfig, ToolCall,
    ToolPolicy, ToolPreset, TruncationConfig,
};
use serde_json::{json, Value};
use support::TempWorkspace;

type ExampleResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
const CHECK: &str = "test -f RELEASE.md && printf 'fixture-present\\n'";
const TOTAL: Duration = Duration::from_secs(600);
const GRACE: Duration = Duration::from_secs(10);

/// Minimal credential source for demonstrating the Codex adapter.
///
/// Production hosts should persist token metadata and perform an actual refresh
/// exchange instead of repeatedly returning environment-provided credentials.
struct EnvCodexCredentials {
    access_token: String,
    account_id: String,
}

#[async_trait]
impl CredentialSource for EnvCodexCredentials {
    async fn credential(&self) -> std::result::Result<BearerCredential, CredentialError> {
        Ok(BearerCredential {
            access_token: self.access_token.clone(),
            expires_at: None,
        })
    }

    async fn refresh(&self) -> std::result::Result<BearerCredential, CredentialError> {
        // A real host would exchange the refresh token here.
        self.credential().await
    }
}

#[async_trait]
impl CodexCredentialSource for EnvCodexCredentials {
    async fn codex_credential(&self) -> std::result::Result<CodexCredential, CredentialError> {
        Ok(CodexCredential {
            bearer: self.credential().await?,
            account_id: self.account_id.clone(),
        })
    }

    async fn refresh_codex(
        &self,
        _rejected: &str,
    ) -> std::result::Result<CodexCredential, CredentialError> {
        self.codex_credential().await
    }
}

fn env(name: &str) -> ExampleResult<String> {
    std::env::var(name).map_err(|_| format!("set {name} to run this example").into())
}

/// Builds a live provider adapter behind the common model interface.
///
/// Constructing parent and child adapters independently below demonstrates that
/// subagents do not depend on sharing the parent's concrete provider instance.
fn model(provider: &str, override_model: Option<&str>) -> ExampleResult<Arc<dyn Model>> {
    Ok(match provider {
        "anthropic" => Arc::new(
            AnthropicModel::new(override_model.unwrap_or("claude-sonnet-4-5"))
                .api_key(env("ANTHROPIC_API_KEY")?)
                .max_tokens(4096),
        ),
        "openai" => Arc::new(
            OpenAiModel::new(override_model.unwrap_or("gpt-5"))
                .api_key(env("OPENAI_API_KEY")?)
                .max_tokens(4096)
                .reasoning_effort("low")
                .usage_accounting(true),
        ),
        "openrouter" => Arc::new(
            OpenRouterModel::new(override_model.unwrap_or("anthropic/claude-sonnet-4.5"))
                .api_key(env("OPENROUTER_API_KEY")?)
                .max_tokens(4096),
        ),
        "codex" => Arc::new(
            OpenAiCodexModel::new(
                override_model.unwrap_or("gpt-5-codex"),
                Arc::new(EnvCodexCredentials {
                    access_token: env("CODEX_ACCESS_TOKEN")?,
                    account_id: env("CODEX_ACCOUNT_ID")?,
                }),
            )
            .reasoning_effort("low"),
        ),
        _ => return Err("provider must be anthropic, openai, openrouter, or codex".into()),
    })
}

/// Defines the security boundary inherited by the parent and child agents.
///
/// The process exception is intentionally exact: even though coding tools are
/// installed, the model cannot mutate files, publish, browse the web, or run an
/// arbitrary command. Tool availability alone never grants permission to use it.
fn policy() -> ToolPolicy {
    ToolPolicy::new().rule(|call: &ToolCall| {
        let allowed = match call.name.as_str() {
            "read_file"
            | "list_dir"
            | "grep"
            | "glob"
            | "skill"
            | "subagent"
            | "workflow"
            | "mcp_select_tool"
            | "mcp__release__check" => true,
            "process" => call.arguments["action"] == "spawn" && call.arguments["command"] == CHECK,
            _ => false,
        };
        if allowed {
            PolicyOutcome::Allow
        } else {
            PolicyOutcome::Deny(
                "Review only: publishing, mutations and arbitrary external actions forbidden"
                    .into(),
            )
        }
    })
}

#[tokio::main]
async fn main() -> ExampleResult {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--release-mcp") {
        return serve_mcp();
    }
    if args.is_empty() || args.len() > 2 {
        return Err("usage: live_host_assembly <anthropic|openai|openrouter|codex> [model]".into());
    }
    let parent = model(&args[0], args.get(1).map(String::as_str))?;
    // Use a separate live adapter for delegated work so this example exercises
    // the same provider boundary a host with independently configured models uses.
    let child = model(&args[0], args.get(1).map(String::as_str))?;
    let workspace = TempWorkspace::new("live-release");
    std::fs::write(
        workspace.path().join("RELEASE.md"),
        "# v1.0 release candidate\nUnit tests: passed locally.\nMigration docs: missing.\nSecurity review: pending.\nDecision: do not publish yet.\n",
    )?;
    // Keep both the review fixture and persistent harness state inside the
    // temporary workspace so a run is isolated and removed on process exit.
    let harness = Harness::builder()
        .workspace(workspace.path())
        .state_dir(workspace.path().join(".orca"))
        .build()?;
    let memory = harness.memory()?;
    memory.save(
        "Release reviews must distinguish evidence from unverified claims.",
        "preference",
        false,
        "host",
    )?;
    let skills = Skills::new(
        harness.workspace().root(),
        Some(harness.state_dir().into()),
        None,
    );
    let skill = skills.scaffold("release_checklist", SkillDestination::Workspace)?;
    std::fs::write(
        skill,
        "---\nname: release_checklist\ndescription: Review release readiness\n---\nRead RELEASE.md. Report evidence, blockers, and next steps. Never publish.\n",
    )?;
    skills.reload();
    let mcp = harness.mcp();
    // Run the assembled host in a scope that always reaches the disconnect below,
    // including initialization, model, persistence, and deadline failures.
    let result = async {
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
        if !status.healthy || status.tool_count != 1 {
            return Err("local MCP initialization failed".into());
        }
        let agent = harness
            .agent(parent)
            .name("live-release-review")
            .system_prompt(
                "Review only the seeded release. Use tools for evidence. Delegate bounded read-only tasks; never publish. End your turn while detached work is pending; the host will continue you with results. Do not resubmit completed tasks.",
            )
            .tools(ToolPreset::Coding)
            .policy(policy())
            .memory(MemoryConfig::new(memory))
            .skills(skills)
            .mcp(mcp.clone())
            .processes(ProcessConfig::new().max_processes(2))
            .subagents(
                SubagentConfig::new()
                    .background_limit(3)
                    .max_depth(1)
                    .workflows(true)
                    .model(SubagentModel::new(
                        "reviewer",
                        "Dedicated same-provider release reviewer",
                        child,
                    ))
                    .system_prompt(
                        "Read-only release reviewer. Read RELEASE.md if needed. Answer the assigned task concisely, without delegation or shell commands.",
                    )
                    .limits(Limits {
                        max_steps: 8,
                        ..Default::default()
                    }),
            )
            .limits(Limits {
                max_steps: 24,
                ..Default::default()
            })
            .tool_retry(RetryConfig::attempts(2).backoff_ms(250))
            .model_retry(RetryConfig::attempts(2).backoff_ms(500))
            .truncation(TruncationConfig {
                max_string_chars: 8000,
                ..Default::default()
            })
            .events(true)
            .usage(true)
            .build()?;
        let session = agent.new_session().persistent().open()?;
        println!(
            "Temporary workspace: {}; persistent session: {:?}",
            workspace.path().display(),
            session.id()
        );
        session
            .subagents()
            .unwrap()
            .settings()
            .set_default_model(Some("reviewer".into()));
        let result = tokio::time::timeout(TOTAL, review(&session)).await;
        // Cleanup happens on failures and on the total deadline as well.
        let shutdown = session.shutdown(GRACE).await;
        match result {
            Ok(result) => result?,
            Err(_) => {
                return Err(
                    "partial review: ten-minute deadline exceeded; detached work cancelled"
                        .into(),
                )
            }
        }
        shutdown?;
        if !harness.sessions().list().iter().any(|s| Some(s.meta.id.clone()) == session.id()) {
            return Err("persistent session was not discoverable".into());
        }
        Ok(())
    }.await;
    mcp.disconnect("release");
    result
}

/// Drives foreground turns until every acknowledged background job is delivered.
///
/// Background notifications are wakeups. Durable session state remains the source
/// of truth because broadcasts can lag behind terminal worker/process state.
async fn review(session: &Session) -> ExampleResult {
    // Subscribe before the first turn so fast background jobs cannot finish before
    // the host has a receiver for their completion notifications.
    let mut notifications = session.notifications();
    let prompt = format!(
        r#"Produce a release readiness review, not a release.
1. Read RELEASE.md with read_file and load skill release_checklist with skill.
2. Select and call mcp__release__check for tag v1.0. This is a local fixture tag check, not proof of test success.
3. Spawn one detached subagent with background=true, timeoutSeconds=240 using model reviewer to independently identify blockers from RELEASE.md.
4. Submit workflow action=run with this valid graph, maxStages=2, timeoutSeconds=240:
[{{"id":"audit","prompt":"Read RELEASE.md and summarize missing evidence.","model":"reviewer"}},
 {{"id":"recommend","needs":["audit"],"prompt":"Recommend next steps from: {{{{ stages.audit.output }}}}","model":"reviewer"}}]
5. Use process action=spawn, command={CHECK:?}, notifyOnExit=true, to check fixture presence. No other shell commands are permitted.
Do not poll detached tasks. End your turn if waiting; on continuation incorporate their outcomes and report evidence, blockers and next steps. Do each requested capability once; never claim an unperformed check succeeded."#
    );
    let mut request = RunRequest::new(prompt);
    let mut delivered = BTreeSet::new();
    let mut failures = Vec::new();
    let mut reported_processes = BTreeSet::new();
    for turn in 0..8 {
        request.deadline = Some(Duration::from_secs(180));
        let mut handle = session.start(request)?;
        // Event streams must be drained concurrently; waiting for the outcome first
        // could apply backpressure and prevent the run from making progress.
        let mut events = handle.take_events().unwrap();
        let reader = tokio::spawn(async move {
            let mut count = 0;
            while events.recv().await.is_some() {
                count += 1;
            }
            count
        });
        let outcome = handle.outcome().await?;
        println!(
            "Turn {turn}: usage={:?}, metered_steps={}, events={}, dropped={}, persistence={:?}",
            outcome.usage,
            outcome.metered_steps,
            reader.await?,
            outcome.dropped_events,
            outcome.persistence
        );
        // Print partial accounting before propagating either execution or persistence failure.
        println!("Outcome: {:?}", outcome.execution);
        outcome.into_result()?;
        // Derive expected job IDs from successful tool results, not model intent.
        // This prevents failed or merely proposed spawns from blocking quiescence.
        let expected: BTreeSet<u64> = session
            .messages()
            .await
            .iter()
            .filter_map(|m| {
                if let Message::Tool { results } = m {
                    Some(results)
                } else {
                    None
                }
            })
            .flatten()
            .filter(|r| !r.is_error)
            .filter_map(|r| match r.tool_name.as_str() {
                "subagent" => r.output["spawnId"].as_u64(),
                "workflow" => r.output["runId"].as_u64(),
                _ => None,
            })
            .collect();
        loop {
            // Notifications are wakeups, not a substitute for status/inbox checks.
            loop {
                match notifications.try_recv() {
                    Ok(n) => observe(n, &mut delivered, &mut failures),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(e) => {
                        return Err(format!(
                            "partial review: cannot verify notification delivery: {e}"
                        )
                        .into())
                    }
                }
            }
            let workers = session.subagents().unwrap().active();
            let workflows = session.workflows().unwrap().runs();
            let processes = session.processes().unwrap().list()?;
            let busy = !workers.is_empty()
                || workflows.iter().any(|w| w.outcome.is_none())
                || processes.iter().any(|p| p.running);
            if !busy {
                let process_completed = processes
                    .iter()
                    .any(|p| !reported_processes.contains(&p.id));
                if session.pending_completions() > 0 || process_completed {
                    reported_processes.extend(processes.iter().map(|p| p.id.clone()));
                    request = if process_completed {
                        // Process exits are host-observed rather than inserted into
                        // the completion inbox, so carry their status explicitly.
                        RunRequest::new(format!("Host process status: {processes:?}. Incorporate completed review work; do not spawn again."))
                    } else {
                        // A continuation consumes pending subagent/workflow results
                        // without adding another user-authored task to the session.
                        RunRequest::continuation()
                    };
                    break;
                }
                // Status becomes terminal BEFORE the completion enters the inbox.
                // Do not declare quiescence until every acknowledged job was delivered.
                if expected.is_subset(&delivered) {
                    if !failures.is_empty() {
                        return Err(
                            format!("partial review: detached failures {failures:?}").into()
                        );
                    }
                    return verify(session).await;
                }
            }
            tokio::select! {
                notification = notifications.recv() => {
                    match notification {
                        Ok(n) => {
                            observe(n, &mut delivered, &mut failures);
                        }
                        Err(e) => return Err(format!("partial review: notification gap: {e}").into()),
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
    }
    Err("partial review: continuation budget exhausted".into())
}

fn observe(
    notification: BackgroundNotification,
    delivered: &mut BTreeSet<u64>,
    failures: &mut Vec<String>,
) {
    println!("Background: {notification:?}");
    match notification {
        BackgroundNotification::CompletionsDelivered { spawn_ids } => delivered.extend(spawn_ids),
        BackgroundNotification::SubagentFinished(n) => {
            if let Err(error) = n.result {
                failures.push(format!("worker {}: {error}", n.spawn.id));
            }
        }
        _ => {}
    }
}

/// Verifies observed tool exchanges and terminal state rather than trusting the
/// model's prose claims about which capabilities it exercised.
async fn verify(session: &Session) -> ExampleResult {
    let messages = session.messages().await;
    let mut exercised = BTreeSet::new();
    for message in &messages {
        if let Message::Assistant { tool_calls, .. } = message {
            for call in tool_calls {
                let succeeded = messages.iter().any(|m| {
                    matches!(m, Message::Tool { results }
                    if results.iter().any(|r| r.call_id == call.id && !r.is_error))
                });
                if succeeded {
                    match call.name.as_str() {
                        "read_file" if call.arguments.to_string().contains("RELEASE.md") => {
                            exercised.insert("fixture");
                        }
                        "skill" if call.arguments["name"] == "release_checklist" => {
                            exercised.insert("skill");
                        }
                        "mcp__release__check" if call.arguments["tag"] == "v1.0" => {
                            exercised.insert("mcp");
                        }
                        "subagent" if call.arguments["background"] == true => {
                            exercised.insert("subagent");
                        }
                        "workflow" if call.arguments["action"] == "run" => {
                            exercised.insert("workflow");
                        }
                        "process"
                            if call.arguments["action"] == "spawn"
                                && call.arguments["command"] == CHECK =>
                        {
                            exercised.insert("process");
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    let required = BTreeSet::from(["fixture", "skill", "mcp", "subagent", "workflow", "process"]);
    println!("Verified successful tool exchanges: {exercised:?}");
    if exercised != required {
        return Err(format!(
            "partial review: missing capabilities {:?}",
            required.difference(&exercised).collect::<Vec<_>>()
        )
        .into());
    }
    let workflows = session.workflows().unwrap().runs();
    if workflows.is_empty() || workflows.iter().any(|w| w.state != RunState::Done) {
        return Err(format!("partial review: workflow outcomes {workflows:?}").into());
    }
    let processes = session.processes().unwrap().list()?;
    if processes.is_empty()
        || processes
            .iter()
            .any(|p| p.running || p.exit_code != Some(0))
    {
        return Err(format!("partial review: process outcomes {processes:?}").into());
    }
    println!("Release review capability checks passed (not a release approval).");
    Ok(())
}

/// Serves a single-tool, line-delimited JSON-RPC fixture over stdio.
///
/// Keeping this server in the current executable makes the MCP transport real
/// while ensuring the release check remains deterministic and side-effect free.
fn serve_mcp() -> ExampleResult {
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
                json!({"content":[{"type":"text","text":format!("verified tag {}", request["params"]["arguments"]["tag"].as_str().unwrap())}],"isError":false})
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
