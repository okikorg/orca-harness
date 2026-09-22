mod runs;
#[cfg(test)]
use runs::run_interactive_context;
use runs::{process_notification_prompt, rotate_for_clear, run_and_report};

use super::context::{context_from, repair_dangling_tool_calls, rewind_cut};
use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;

use orca_harness_core::{Agent, CancellationToken, Context, Model};
use orca_harness_extensions::{
    compact, CompactConfig, ContextCapacity, HarnessEvent, SessionHandler, TruncationStore,
};
use orca_harness_tools::{CompletionInbox, FileGuard, SubagentManager, TodoList};

use crate::msg::{RunId, UiMsg, WorkerCmd};
use crate::{config, mcp, skills, spawn_window_probe, Endpoint, Planning};

struct CancelSubagentsOnDrop(SubagentManager);

impl Drop for CancelSubagentsOnDrop {
    fn drop(&mut self) {
        self.0.cancel_all();
    }
}

/// A path shown to the user: relative to the working directory when it
/// is inside it, absolute otherwise. Transcript notices stay short.
fn workspace_relative(path: &std::path::Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok())
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Owns the Agent and the conversation; runs prompts sent by the UI.
/// `build` produces a fresh agent for the current endpoint; the
/// conversation context survives model and provider swaps. The boolean requests
/// local-tool preservation only when publishing a completed MCP reload.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn worker<F>(
    built: (
        Agent<Arc<dyn Model>>,
        Arc<orca_harness_tools::SubagentTool<Arc<dyn Model>>>,
    ),
    system: String,
    mut endpoint: Endpoint,
    mut build: F,
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
    process_generation: Arc<std::sync::atomic::AtomicU64>,
    subagent_manager: SubagentManager,
    completions: CompletionInbox,
    ui: mpsc::UnboundedSender<UiMsg>,
) where
    F: FnMut(
        &Endpoint,
        bool,
    ) -> (
        Agent<Arc<dyn Model>>,
        Arc<orca_harness_tools::SubagentTool<Arc<dyn Model>>>,
    ),
{
    let (mut agent, mut subagent) = built;
    // Rebuilding the parent agent replaces its registered tool, but a
    // sidekick must keep the exact tool instance that owns its retained
    // context until explicit stop or session teardown.
    let mut sidekicks = HashMap::new();
    let _subagent_shutdown = CancelSubagentsOnDrop(subagent_manager.clone());
    let mut user_shell_call_id = 0_u64;
    let mut background_subagent_sequence = 0_u64;
    let mut login_attempt = 0_u64;
    // Skills /refine applied, newest last; RefineUndo pops and deletes.
    let mut refine_applied: Vec<crate::refine::Applied> = Vec::new();
    let mut login_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut mcp_reload = super::mcp_reload::McpReload::default();
    spawn_window_probe(&endpoint, ui.clone(), context_capacity.clone());
    while let Some(command) = commands.recv().await {
        match command {
            WorkerCmd::Run {
                id,
                prompt,
                images,
                cancel,
            } => {
                let _ = ui.send(UiMsg::RunStarted {
                    id: id.clone(),
                    cancel: cancel.clone(),
                });
                // Briefing the model is not news for the user: the
                // /mode line already said the session is read-only,
                // and whether a plan file appears is up to the agent.
                planning.open_episode(&mut context);
                context.push_user_with_images(&prompt, images);
                if !run_and_report(&agent, &mut context, &cancel, session.as_deref(), &ui, id).await
                {
                    return;
                }
            }
            WorkerCmd::Shell {
                id,
                command,
                working_dir,
                cancel,
            } => {
                use orca_harness_core::{Tool, ToolCall, ToolContext, ToolResult};

                let _ = ui.send(UiMsg::RunStarted {
                    id: id.clone(),
                    cancel: cancel.clone(),
                });

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
                if ui.send(UiMsg::ShellDone { id }).is_err() {
                    return;
                }
            }
            WorkerCmd::BackgroundProcess {
                generation,
                sequence,
                notification,
            } => {
                if !is_current_process_generation(generation, &process_generation) {
                    continue;
                }
                let id = RunId::BackgroundProcess {
                    generation,
                    sequence,
                };
                let prompt = process_notification_prompt(&notification);
                let cancel = CancellationToken::new();
                if ui
                    .send(UiMsg::RunStarted {
                        id: id.clone(),
                        cancel: cancel.clone(),
                    })
                    .is_err()
                {
                    return;
                }
                context.push_user(&prompt);
                if !run_and_report(&agent, &mut context, &cancel, session.as_deref(), &ui, id).await
                {
                    return;
                }
            }
            WorkerCmd::BackgroundSubagentsReady => {
                // Already delivered mid-run, or cleared: nothing to wake for.
                if !completions.consume_wakeup() {
                    continue;
                }
                background_subagent_sequence += 1;
                let id = RunId::BackgroundSubagents {
                    sequence: background_subagent_sequence,
                };
                let cancel = CancellationToken::new();
                if ui
                    .send(UiMsg::RunStarted {
                        id: id.clone(),
                        cancel: cancel.clone(),
                    })
                    .is_err()
                {
                    return;
                }
                // No prompt is pushed here: the agent's CompletionDelivery
                // extension drains the inbox into one user turn at the first
                // model call, the same path a parent mid-run takes.
                if !run_and_report(&agent, &mut context, &cancel, session.as_deref(), &ui, id).await
                {
                    return;
                }
            }
            WorkerCmd::SidekickStart { task, tier } => {
                let mut request = orca_harness_tools::SubagentRequest::new(task.clone());
                if let Some(tier) = tier {
                    let Some(model) = subagent.max_depth_preferred_model(&tier) else {
                        let _ = ui.send(UiMsg::Notice(format!(
                            "sidekick start failed: no saved preferred {tier} model"
                        )));
                        continue;
                    };
                    request = request.model(model);
                }
                let call_id = format!("sidekick-start-{}", background_subagent_sequence);
                background_subagent_sequence += 1;
                let _ = ui.send(UiMsg::Event(HarnessEvent::ToolCall {
                    tool_call_id: call_id.clone(),
                    tool_name: "subagent".into(),
                    input: serde_json::json!({"task": task, "persistent": true}),
                }));
                match subagent.start_sidekick(request) {
                    Ok(ack) => {
                        let spawn_id = ack.spawn_id;
                        let identity = ack.identity.clone();
                        let started_call_id = format!("sidekick-{spawn_id}");
                        let _ = ui.send(UiMsg::SubagentStarted {
                            id: spawn_id,
                            parent_id: None,
                            depth: 0,
                            call_id: started_call_id,
                            task,
                            identity,
                            run: None,
                            stage: None,
                        });
                        let _ = ui.send(UiMsg::Event(HarnessEvent::ToolResult {
                            tool_call_id: call_id,
                            tool_name: "subagent".into(),
                            output: ack.into_value(),
                            is_error: false,
                        }));
                        sidekicks.insert(spawn_id, subagent.clone());
                    }
                    Err(error) => {
                        let message = format!("sidekick start failed: {error}");
                        let _ = ui.send(UiMsg::Event(HarnessEvent::ToolResult {
                            tool_call_id: call_id,
                            tool_name: "subagent".into(),
                            output: serde_json::json!({"error": message}),
                            is_error: true,
                        }));
                        let _ = ui.send(UiMsg::Notice(message));
                    }
                }
            }
            WorkerCmd::SidekickStop { spawn_id } => match sidekicks
                .get(&spawn_id)
                .unwrap_or(&subagent)
                .stop_sidekick(spawn_id)
            {
                Ok(()) => {
                    sidekicks.remove(&spawn_id);
                    let _ = ui.send(UiMsg::Event(HarnessEvent::ToolResult {
                    tool_call_id: format!("sidekick-stop-{spawn_id}"), tool_name: "subagent".into(),
                    output: serde_json::json!({"spawnId": spawn_id, "status": "stopped", "termination": "stopped"}), is_error: false,
                }));
                }
                Err(error) => {
                    let _ = ui.send(UiMsg::Notice(format!("sidekick stop failed: {error}")));
                }
            },
            WorkerCmd::Clear => {
                let (fresh, new_session_id) = match rotate_for_clear(&system, session.as_deref()) {
                    Ok(rotated) => rotated,
                    Err(err) => {
                        let _ = ui.send(UiMsg::Notice(format!(
                            "session not cleared: could not preserve history: {err}"
                        )));
                        continue;
                    }
                };
                context = fresh;
                // The plan belonged to the conversation being cleared,
                // and so did every "I have read this file": the model
                // that did the reading is gone. The planning episode
                // ends too, so a fresh conversation names a fresh file
                // rather than appending to the last one's plan.
                todos.clear();
                files.clear();
                planning.area.end();
                completions.reset();
                sidekicks.clear();
                // The manager cancellation above stops detached subagents. A
                // fresh agent also drops process and interpreter tools, whose
                // Drop implementations stop their background work. Do this
                // before acknowledging success to the UI.
                (agent, subagent) = build(&endpoint, false);
                let _ = ui.send(UiMsg::SessionCleared { id: new_session_id });
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
            WorkerCmd::Refine => {
                // A one-shot proposer pass over a scratch context: the
                // user's conversation is read, never written. The model
                // is built fresh from the live endpoint so provider and
                // model switches are always respected.
                let model = endpoint.build_model_for_ui(Some(ui.clone()));
                let existing: Vec<(String, String)> = skills
                    .catalog()
                    .into_iter()
                    .map(|entry| (entry.name, entry.description))
                    .collect();
                let result =
                    crate::refine::run_proposer(model.as_ref(), context.messages(), &existing)
                        .await;
                let result = match result {
                    // Declining is success, not silence: say why and stop.
                    Ok(crate::refine::Refined::Nothing(reason)) => {
                        let reason = match reason.is_empty() {
                            true => String::new(),
                            false => format!(": {reason}"),
                        };
                        let _ = ui.send(UiMsg::Notice(format!(
                            "refine: no skill proposed — nothing in this trajectory is worth \
                             packaging{reason}"
                        )));
                        continue;
                    }
                    Ok(crate::refine::Refined::Skill(outcome)) => Ok(outcome),
                    Err(err) => Err(err),
                };
                let proposal = result.as_ref().ok().map(|outcome| outcome.proposal.clone());
                if ui.send(UiMsg::RefineDone(Box::new(result))).is_err() {
                    return;
                }
                let Some(proposal) = proposal else { continue };
                // Accept/reject rides the standard tool-approval gate
                // (y/n/a/A) rather than a bespoke prompt.
                let (respond, decision) = tokio::sync::oneshot::channel();
                let request = crate::msg::ApprovalRequest {
                    tool_name: "refine".into(),
                    detail: format!("apply skill {}", proposal.name),
                    yes_no: true,
                    respond,
                };
                if ui.send(UiMsg::Approval(request)).is_err() {
                    return;
                }
                use crate::msg::ApprovalResponse as R;
                let approved = matches!(
                    decision.await,
                    Ok(R::AllowOnce | R::AllowAlways | R::AllowAlwaysSave)
                );
                if !approved {
                    let _ = ui.send(UiMsg::Notice(format!(
                        "refine: proposal {} discarded",
                        proposal.name
                    )));
                    continue;
                }
                let applied = skills
                    .project_root()
                    .map(std::path::Path::to_path_buf)
                    .ok_or_else(|| "no project skills folder to apply into".to_string())
                    .and_then(|root| crate::refine::apply(&root, &proposal));
                let notice = match applied {
                    Ok(applied) => {
                        skills.reload();
                        let notice = format!(
                            "applied skill {} → {} · /refine undo reverts it",
                            proposal.name,
                            workspace_relative(&applied.dir)
                        );
                        refine_applied.push(applied);
                        notice
                    }
                    Err(err) => format!("refine apply failed: {err}"),
                };
                let _ = ui.send(UiMsg::Notice(notice));
            }
            WorkerCmd::RefineUndo => {
                let notice = match refine_applied.pop() {
                    None => "nothing to undo: /refine has applied no skills".to_string(),
                    Some(applied) => match crate::refine::undo(&applied) {
                        Ok(()) => {
                            skills.reload();
                            format!("removed {}", workspace_relative(&applied.dir))
                        }
                        Err(err) => {
                            let notice = format!("undo failed: {err}");
                            refine_applied.push(applied);
                            notice
                        }
                    },
                };
                let _ = ui.send(UiMsg::Notice(notice));
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
                        completions.reset();
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
            WorkerCmd::ListSubagentModels {
                request_id,
                provider,
            } => {
                let candidate = crate::subagent_models::provider_endpoint(&endpoint, provider);
                let ui = ui.clone();
                tokio::spawn(async move {
                    let result = candidate.list_models().await.map_err(|e| e.to_string());
                    let _ = ui.send(UiMsg::Models { request_id, result });
                });
            }
            WorkerCmd::SetSubagentModel {
                tier,
                provider,
                model,
            } => {
                match crate::subagent_models::save_assignment(
                    &endpoint,
                    &subagent_manager,
                    &tier,
                    provider,
                    model,
                ) {
                    Ok(_) => {
                        (agent, subagent) = build(&endpoint, false);
                        let result = config::save_subagent_settings(&endpoint.subagent_settings);
                        let note = match result {
                            Ok(_) => format!("subagent {tier} model saved; applies to new spawns"),
                            Err(e) => {
                                format!("subagent model saved, routing settings save failed: {e}")
                            }
                        };
                        let _ = ui.send(UiMsg::Notice(note));
                    }
                    Err(e) => {
                        let _ = ui.send(UiMsg::Notice(format!("subagent model save failed: {e}")));
                    }
                }
            }
            WorkerCmd::ListModels { request_id, filter } => {
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
                    let _ = ui.send(UiMsg::Models { request_id, result });
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
                        let mut candidate = endpoint.clone();
                        candidate.provider = provider;
                        candidate.base_url = provider.base_url().into();
                        candidate.api_key = None;
                        candidate.reasoning_effort = None;
                        let requested = config::stored_model(provider.label());
                        candidate.model = match candidate.catalog_model(requested.as_deref()).await
                        {
                            Ok(model) => model,
                            Err(error) => {
                                let _ = ui.send(UiMsg::Notice(format!(
                                    "login succeeded, but model catalog failed: {error}"
                                )));
                                continue;
                            }
                        };
                        endpoint = candidate;
                        let _ = config::save_provider(provider.label());
                        (agent, subagent) = build(&endpoint, false);
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
            WorkerCmd::SetModel {
                id,
                reasoning_effort,
            } => {
                endpoint.model = id;
                endpoint.reasoning_effort = reasoning_effort;
                // Best-effort preference cache; a failed write only means
                // the next session starts on the provider default.
                let _ = config::save_model(endpoint.provider.label(), &endpoint.model);
                (agent, subagent) = build(&endpoint, false);
                spawn_window_probe(&endpoint, ui.clone(), context_capacity.clone());
                if ui
                    .send(UiMsg::ModelChanged {
                        id: endpoint.model.clone(),
                        reasoning_effort: endpoint.reasoning_effort.clone(),
                    })
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
                let mut candidate = endpoint.clone();
                candidate.provider = provider;
                candidate.base_url = provider.base_url().into();
                candidate.api_key = api_key.or_else(|| provider.resolve_key());
                candidate.reasoning_effort = None;
                let requested = config::stored_model(provider.label());
                candidate.model = match candidate.catalog_model(requested.as_deref()).await {
                    Ok(model) => model,
                    Err(error) => {
                        let _ = ui.send(UiMsg::Notice(format!(
                            "provider model catalog failed: {error}"
                        )));
                        continue;
                    }
                };
                endpoint = candidate;
                let _ = config::save_provider(provider.label());
                (agent, subagent) = build(&endpoint, false);
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
                (agent, subagent) = build(&endpoint, false);
            }
            WorkerCmd::ReloadMcp => {
                mcp_reload.request(&mcp, &ui, &command_tx);
            }
            WorkerCmd::McpReloaded { notices } => {
                let pending = mcp_reload.finish();
                for line in notices {
                    let _ = ui.send(UiMsg::Notice(line));
                }
                // Catalog publication must not kill local processes or REPL state.
                (agent, subagent) = build(&endpoint, true);
                let inventory = mcp.startup_inventory();
                let _ = ui.send(UiMsg::Notice(format!(
                    "MCP initialization complete · {} servers · {} tools available · /mcp for status",
                    inventory.servers, inventory.tools,
                )));
                if pending {
                    mcp_reload.request(&mcp, &ui, &command_tx);
                } else {
                    let _ = ui.send(UiMsg::McpConnecting(false));
                }
            }
            WorkerCmd::TestPlugin { path } => super::plugin::test(&path, &ui).await,
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
                (agent, subagent) = build(&endpoint, false);
            }
            WorkerCmd::ReloadSkills => {
                // Rescan (cheap) and rebuild, so a skill created or
                // toggled while the session is open reaches the model's
                // catalog. The user asked, so always answer: a scan that
                // found nothing new must not look like a scan that never
                // ran (script edits don't change the catalog at all).
                let lines = skills.reload();
                match lines.is_empty() {
                    true => {
                        let count = skills.catalog().len();
                        let _ = ui.send(UiMsg::Notice(format!(
                            "skills rescanned · {count} loaded · no changes"
                        )));
                    }
                    false => {
                        for line in lines {
                            let _ = ui.send(UiMsg::Notice(line));
                        }
                    }
                }
                (agent, subagent) = build(&endpoint, false);
            }
        }
    }
}

fn is_current_process_generation(
    event_generation: u64,
    current: &std::sync::atomic::AtomicU64,
) -> bool {
    current.load(std::sync::atomic::Ordering::Acquire) == event_generation
}

#[cfg(test)]
mod tests;
