use super::context::{context_from, repair_dangling_tool_calls, rewind_cut};
use std::sync::Arc;

use tokio::sync::mpsc;

use orca_harness_core::{Agent, CancellationToken, Context, HarnessError, Model};
use orca_harness_extensions::{
    compact, CompactConfig, ContextCapacity, HarnessEvent, SessionHandler, TruncationStore,
};
use orca_harness_tools::{FileGuard, TodoList};

use crate::msg::{UiMsg, WorkerCmd};
use crate::{config, mcp, skills, spawn_window_probe, Endpoint, Planning};

async fn run_interactive_context(
    agent: &Agent<Arc<dyn Model>>,
    context: &mut Context,
    cancel: &CancellationToken,
    continue_at_step_limit: bool,
) -> Result<String, HarnessError> {
    loop {
        match agent.run_context(context, cancel.clone()).await {
            Err(HarnessError::StepLimitExceeded) if continue_at_step_limit => {
                continue;
            }
            result => return result,
        }
    }
}

/// Owns the Agent and the conversation; runs prompts sent by the UI.
/// `build` produces a fresh agent for the current endpoint; the
/// conversation context survives model and provider swaps.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn worker<F>(
    mut agent: Agent<Arc<dyn Model>>,
    system: String,
    mut endpoint: Endpoint,
    build: F,
    store: TruncationStore,
    context_capacity: ContextCapacity,
    mcp: mcp::McpServers,
    skills: skills::Skills,
    session: Option<Arc<SessionHandler>>,
    todos: TodoList,
    files: FileGuard,
    planning: Planning,
    mut context: Context,
    mut commands: mpsc::UnboundedReceiver<WorkerCmd>,
    command_tx: mpsc::UnboundedSender<WorkerCmd>,
    ui: mpsc::UnboundedSender<UiMsg>,
) where
    F: Fn(&Endpoint) -> Agent<Arc<dyn Model>>,
{
    let mut user_shell_call_id = 0_u64;
    let mut login_attempt = 0_u64;
    let mut login_task: Option<tokio::task::JoinHandle<()>> = None;
    spawn_window_probe(&endpoint, ui.clone(), context_capacity.clone());
    while let Some(command) = commands.recv().await {
        match command {
            WorkerCmd::Run {
                prompt,
                images,
                cancel,
            } => {
                // Briefing the model is not news for the user: the
                // /mode line already said the session is read-only,
                // and whether a plan file appears is up to the agent.
                planning.open_episode(&mut context);
                context.push_user_with_images(&prompt, images);
                let result = run_interactive_context(
                    &agent,
                    &mut context,
                    &cancel,
                    crate::extensions::enabled("long-session"),
                )
                .await;
                repair_dangling_tool_calls(&mut context);
                // The repair lands after on_agent_end fired; catch up so
                // the file never ends in dangling tool calls.
                if let Some(session) = &session {
                    session.sync(&context);
                }
                let done = UiMsg::RunDone(result.map_err(|e| e.to_string()));
                if ui.send(done).is_err() {
                    return;
                }
            }
            WorkerCmd::Shell {
                command,
                working_dir,
                cancel,
            } => {
                use orca_harness_core::{Tool, ToolCall, ToolContext, ToolResult};

                user_shell_call_id += 1;
                let call = ToolCall {
                    id: format!("user-shell-{user_shell_call_id}"),
                    name: "shell".into(),
                    arguments: serde_json::json!({ "command": command }),
                };
                context.push_user(format!(
                    "!{}",
                    call.arguments["command"].as_str().unwrap_or_default()
                ));
                context.push_assistant_tool_calls(None, vec![call.clone()]);
                let _ = ui.send(UiMsg::Event(HarnessEvent::ToolCall {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    input: call.arguments.clone(),
                }));

                let tool = orca_harness_tools::ShellTool::local().working_dir(working_dir);
                let tool_context = ToolContext {
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    cancellation: cancel,
                    deadline: None,
                };
                let result = match tool.call(call.arguments.clone(), &tool_context).await {
                    Ok(output) => ToolResult::ok(&call, output),
                    Err(err) => ToolResult::error(&call, err.to_string()),
                };
                let _ = ui.send(UiMsg::Event(HarnessEvent::ToolResult {
                    tool_call_id: result.call_id.clone(),
                    tool_name: result.tool_name.clone(),
                    output: result.output.clone(),
                    is_error: result.is_error,
                }));
                context.append_tool_results(vec![result]);
                if let Some(session) = &session {
                    session.sync(&context);
                }
                if ui.send(UiMsg::ShellDone).is_err() {
                    return;
                }
            }
            WorkerCmd::Clear => {
                context = Context::new();
                context.push_system(&system);
                // The plan belonged to the conversation being cleared,
                // and so did every "I have read this file": the model
                // that did the reading is gone. The planning episode
                // ends too, so a fresh conversation names a fresh file
                // rather than appending to the last one's plan.
                todos.clear();
                files.clear();
                planning.area.end();
                if let Some(session) = &session {
                    // Same session, emptied in place: /clear does not
                    // litter the sessions directory with rotations.
                    match session.reset() {
                        Ok(()) => {
                            let _ = ui.send(UiMsg::SessionCleared {
                                id: session.session_id(),
                            });
                        }
                        Err(err) => {
                            let _ =
                                ui.send(UiMsg::Notice(format!("session file not cleared: {err}")));
                        }
                    }
                }
                // A fresh agent drops the old process/pykernel/subagent
                // tools; their Drop kills background process groups and
                // the interpreter, so /clear leaves nothing running.
                agent = build(&endpoint);
            }
            WorkerCmd::Compact => {
                let result = compact(&mut context, &store, &CompactConfig::default())
                    .map_err(|e| e.to_string());
                if let Some(session) = &session {
                    session.sync(&context);
                }
                if ui.send(UiMsg::Compacted(result)).is_err() {
                    return;
                }
            }
            WorkerCmd::Rewind { turns } => {
                let Some((cut, dropped_turns)) = rewind_cut(context.messages(), turns) else {
                    let _ = ui.send(UiMsg::Notice("nothing to rewind".into()));
                    continue;
                };
                let dropped_messages = context.messages().len() - cut;
                let kept = context_from(&context.messages()[..cut]);
                context = kept;
                // A dropped turn may have held the read that made a file
                // overwritable. The model can no longer see it, so the
                // guard must not go on believing it looked.
                files.clear();
                // The context is shorter than what was persisted, so
                // this rewrites the session file rather than appending.
                if let Some(session) = &session {
                    session.sync(&context);
                }
                let notice = format!(
                    "rewound {dropped_turns} turn{} · {dropped_messages} message{} dropped",
                    if dropped_turns == 1 { "" } else { "s" },
                    if dropped_messages == 1 { "" } else { "s" },
                );
                let rewound = UiMsg::ContextRewound {
                    messages: context.messages().to_vec(),
                    notice,
                };
                if ui.send(rewound).is_err() {
                    return;
                }
            }
            WorkerCmd::Fork => {
                let Some(session) = &session else {
                    let _ = ui.send(UiMsg::Notice(
                        "session recording is disabled (--no-session)".into(),
                    ));
                    continue;
                };
                let parent = session.session_id();
                match session.fork() {
                    Ok(id) => {
                        // The new file starts empty: sync writes the
                        // whole carried-over conversation into it.
                        session.sync(&context);
                        let _ = ui.send(UiMsg::SessionForked { id, parent });
                    }
                    Err(err) => {
                        let _ = ui.send(UiMsg::Notice(format!("fork failed: {err}")));
                    }
                }
            }
            WorkerCmd::LoadSession { path } => {
                let Some(session) = &session else {
                    let _ = ui.send(UiMsg::Notice(
                        "session recording is disabled (--no-session)".into(),
                    ));
                    continue;
                };
                match session.switch_to(&path) {
                    Ok(loaded) => {
                        for warning in &loaded.warnings {
                            let _ = ui.send(UiMsg::Notice(warning.clone()));
                        }
                        context = loaded.context;
                        let _ = ui.send(UiMsg::SessionLoaded {
                            id: loaded.meta.id,
                            messages: context.messages().to_vec(),
                        });
                    }
                    Err(err) => {
                        let _ = ui.send(UiMsg::Notice(format!("session load failed: {err}")));
                    }
                }
            }
            WorkerCmd::ListModels { filter } => {
                // Detached: a slow catalog fetch must not wedge the worker
                // (runs and model switches would queue behind it).
                let ui = ui.clone();
                let endpoint = endpoint.clone();
                tokio::spawn(async move {
                    let result = endpoint
                        .list_models()
                        .await
                        .map(|mut models| {
                            if !filter.is_empty() {
                                models.retain(|m| m.id.to_lowercase().contains(&filter));
                            }
                            models
                        })
                        .map_err(|e| e.to_string());
                    let _ = ui.send(UiMsg::Models(result));
                });
            }
            WorkerCmd::LoginProvider { provider } => {
                if let Some(task) = login_task.take() {
                    task.abort();
                }
                login_attempt = login_attempt.wrapping_add(1);
                let attempt = login_attempt;
                let progress_ui = ui.clone();
                let completion_tx = command_tx.clone();
                login_task = Some(tokio::spawn(async move {
                    let result = crate::auth::login(provider, move |message| {
                        let _ = progress_ui.send(UiMsg::Notice(message));
                    })
                    .await;
                    let _ = completion_tx.send(WorkerCmd::LoginFinished {
                        provider,
                        attempt,
                        result,
                    });
                }));
            }
            WorkerCmd::LoginFinished {
                provider,
                attempt,
                result,
            } => {
                if attempt != login_attempt {
                    continue;
                }
                login_task = None;
                match result {
                    Ok(()) => {
                        endpoint.provider = provider;
                        endpoint.base_url = provider.base_url().into();
                        endpoint.api_key = None;
                        endpoint.model = provider.default_model().into();
                        let _ = config::save_provider(provider.label());
                        agent = build(&endpoint);
                        spawn_window_probe(&endpoint, ui.clone(), context_capacity.clone());
                        let _ = ui.send(UiMsg::ProviderChanged {
                            provider,
                            model: endpoint.model.clone(),
                        });
                    }
                    Err(error) => {
                        let _ = ui.send(UiMsg::Notice(format!("login failed: {error}")));
                    }
                }
            }
            WorkerCmd::SetModel { id } => {
                endpoint.model = id;
                // Best-effort preference cache; a failed write only means
                // the next session starts on the provider default.
                let _ = config::save_model(endpoint.provider.label(), &endpoint.model);
                agent = build(&endpoint);
                spawn_window_probe(&endpoint, ui.clone(), context_capacity.clone());
                if ui
                    .send(UiMsg::ModelChanged(endpoint.model.clone()))
                    .is_err()
                {
                    return;
                }
            }
            WorkerCmd::SetProvider { provider, api_key } => {
                if let Some(task) = login_task.take() {
                    task.abort();
                }
                login_attempt = login_attempt.wrapping_add(1);
                endpoint.provider = provider;
                endpoint.base_url = provider.base_url().into();
                endpoint.api_key = api_key.or_else(|| provider.resolve_key());
                endpoint.model = provider.default_model().into();
                let _ = config::save_provider(provider.label());
                agent = build(&endpoint);
                spawn_window_probe(&endpoint, ui.clone(), context_capacity.clone());
                let changed = UiMsg::ProviderChanged {
                    provider,
                    model: endpoint.model.clone(),
                };
                if ui.send(changed).is_err() {
                    return;
                }
            }
            WorkerCmd::ReloadExtensions => {
                // The UI already saved the toggle; build_agent reads the
                // config, so rebuilding is all that is left to do.
                agent = build(&endpoint);
            }
            WorkerCmd::ReloadMcp => {
                // The UI already saved the add/remove; reconnect, report
                // per server, and rebuild so the tool set matches.
                for line in mcp.reload().await {
                    let _ = ui.send(UiMsg::Notice(line));
                }
                agent = build(&endpoint);
            }
            WorkerCmd::InstallSkill { source, here } => {
                match skills.add(&source, here).await {
                    Ok(lines) => {
                        for line in lines {
                            let _ = ui.send(UiMsg::Notice(line));
                        }
                    }
                    Err(err) => {
                        let _ = ui.send(UiMsg::Notice(format!("skills add failed: {err}")));
                    }
                }
                for line in skills.reload() {
                    let _ = ui.send(UiMsg::Notice(line));
                }
                agent = build(&endpoint);
            }
            WorkerCmd::ReloadSkills => {
                // Rescan (cheap) and rebuild, so a skill created or
                // toggled while the session is open reaches the model's
                // catalog. Silent when nothing about the scan changed.
                for line in skills.reload() {
                    let _ = ui.send(UiMsg::Notice(line));
                }
                agent = build(&endpoint);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_harness_core::testing::{call, ScriptedModel};
    use orca_harness_core::{FnTool, Limits, ModelResponse};
    use serde_json::json;

    #[tokio::test]
    async fn long_session_continues_across_bounded_agent_runs() {
        let mut responses = (0..7)
            .map(|index| {
                ModelResponse::tool_calls(vec![call(
                    &format!("call-{index}"),
                    "echo",
                    json!({"index": index}),
                )])
            })
            .collect::<Vec<_>>();
        responses.push(ModelResponse::final_text("finished"));
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(responses));
        let echo = FnTool::new(
            "echo",
            "echo input",
            json!({"type": "object"}),
            |input, _| async move { Ok(input) },
        );
        let agent = Agent::new(model).tool(echo).limits(Limits {
            max_steps: 3,
            ..Limits::default()
        });
        let mut context = Context::new();
        context.push_user("work for a long time");

        let answer = run_interactive_context(&agent, &mut context, &CancellationToken::new(), true)
            .await
            .unwrap();

        assert_eq!(answer, "finished");
    }
}
