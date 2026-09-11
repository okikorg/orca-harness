use super::agent::subagent_extensions;
use super::completions::{ActiveInventory, CompletionDelivery, CompletionInbox};
use super::session::open_session;
use super::worker::worker;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

use orca_harness_core::{Agent, Context, Model};
use orca_harness_extensions::{
    workspace_key, ContextCapacity, EventStream, LongSession, MemoryExtension, MemoryManageTool,
    MemoryModel, MemoryScope, MemorySearchTool, MemoryStore, ReadToolResultTool, SessionHandler,
    Truncation, TruncationStore,
};
use orca_harness_tool_extensions::mcp::McpModel;
use orca_harness_tool_extensions::skills::SkillOnce;
use orca_harness_tool_extensions::web::{
    Firecrawl, UrlPolicy, WebCrawlTool, WebFetchTool, WebSearchTool,
};
use orca_harness_tools::{
    core_tools_with_guard, AskTool, BackgroundStats, BunReplTool, FileGuard, ProcessTool,
    PyKernelTool, SubagentDepth, SubagentManager, SubagentSpawn, TodoList, TodoWriteTool,
    Workspace,
};

use crate::approval::Approval;
use crate::auto_approval::AutoApproval;
use crate::mode::{ModeHandle, PlanGate};
use crate::msg::UiMsg;
use crate::plan::PlanArea;
use crate::subagent_models;
use crate::{
    extensions, headless, instructions, mcp, skills, system_prompt, tui, workspace_scope, Config,
    Endpoint, Planning,
};

/// Headless or interactive. The endpoint (provider, base url, key, model)
/// lives in the worker and can be switched from the TUI at runtime.
pub(crate) async fn run_mode(mut cfg: Config) -> ExitCode {
    let session = if cfg.no_session {
        None
    } else {
        match open_session(&mut cfg) {
            Ok(opened) => Some(opened),
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        }
    };

    let ws = Workspace::new(&cfg.workspace);
    // Scanned before the prompt is built: whether the `skill` tool gets
    // advertised depends on whether any skill was found, and the scan is
    // a handful of read_dir calls.
    let skills = if cfg.bare {
        skills::Skills::default()
    } else {
        skills::Skills::for_session(&cfg.workspace)
    };
    let skill_notices = if cfg.bare {
        Vec::new()
    } else {
        skills.reload()
    };
    // The base prompt is a fixed string with tests asserting its
    // contents; the user's standing instructions are appended here, at
    // the call site, so no AGENTS.md can ever change what that function
    // returns. `/clear` re-pushes this composed string, so instructions
    // survive a reset.
    let instructions = if cfg.bare {
        instructions::Instructions::default()
    } else {
        instructions::Instructions::load(&cfg.workspace)
    };
    let instruction_notices = instructions.notices();
    let mut system = if cfg.bare {
        headless::bare_system_prompt(
            &ws,
            &headless::selected_tool_names(&cfg).unwrap_or_default(),
        )
    } else {
        system_prompt(&ws, cfg.firecrawl_key.is_some())
    };
    if let Some(block) = instructions.block() {
        system.push_str(&block);
    }
    let endpoint = Endpoint::from_config(&cfg);
    let subagent_depth = endpoint.subagent_settings.clone();
    let stats = BackgroundStats::new();
    let mode = ModeHandle::new(cfg.mode());
    let todos = TodoList::new();
    let files = FileGuard::new();
    let plan_area = PlanArea::new();
    let planning = Planning {
        mode: mode.clone(),
        area: plan_area.clone(),
    };
    let memory = match crate::config::memory_path()
        .ok_or_else(|| "no home directory for memory storage".to_string())
        .and_then(|path| MemoryStore::open(path).map_err(|error| error.to_string()))
    {
        Ok(memory) => memory,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let memory_root = workspace_scope(&ws);
    let memory_scope = MemoryScope::new(workspace_key(&memory_root), memory_root);

    if cfg.prompt.is_some() {
        let (handler, resumed) = match session {
            Some((handler, resumed)) => (Some(Arc::new(handler)), resumed),
            None => (None, None),
        };
        for line in &instruction_notices {
            eprintln!("{line}");
        }
        let code = headless::run(
            &cfg,
            endpoint.build_model(),
            subagent_models::choices(&endpoint),
            &ws,
            &system,
            handler,
            resumed,
            &skills,
            &skill_notices,
            &mode,
            &todos,
            &plan_area,
            &memory,
            &memory_scope,
            endpoint.model_retry_counter(),
            subagent_depth.clone(),
        )
        .await;
        return ExitCode::from(code as u8);
    }

    // Interactive: worker task owns the agent; UI owns the terminal.
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let process_generation = Arc::new(AtomicU64::new(0));
    // The concurrency limit is a `/subagents` setting: queued workers start
    // the moment the user raises it.
    let subagent_manager = SubagentManager::from_settings(subagent_depth.clone());
    // Stage outputs live for the session, not for one tool: the model-switch
    // closure below rebuilds every tool, and a run admitted before the switch
    // must still replay after it.
    let workflow_store = orca_harness_tools::WorkflowStore::new();
    let completions =
        CompletionInbox::new(subagent_manager.clone()).with_workflow_store(workflow_store.clone());

    // Session warnings surface as transcript notices; recording failures
    // must be visible but never fatal mid-run.
    let (session, resumed) = match session {
        Some((handler, resumed)) => {
            let ui = ui_tx.clone();
            let handler = handler.on_warn(move |message| {
                let _ = ui.send(UiMsg::Notice(message.to_string()));
            });
            (Some(Arc::new(handler)), resumed)
        }
        None => (None, None),
    };
    let context = match resumed {
        // A recorded transcript already begins with its system prompt.
        // SessionLoaded replays it into the transcript once the TUI
        // starts draining the channel.
        Some(context) => {
            let _ = ui_tx.send(UiMsg::SessionLoaded {
                id: session.as_ref().map(|s| s.session_id()).unwrap_or_default(),
                messages: context.messages().to_vec(),
            });
            context
        }
        None => {
            let mut context = Context::new();
            context.push_system(&system);
            context
        }
    };

    // One store for the whole session: agent rebuilds (model/provider
    // swaps) keep it, so read_tool_result and /compact recovery survive.
    let store = TruncationStore::default();
    let context_capacity = ContextCapacity::default();
    // Connect the configured MCP servers before the first build so their
    // tools are in the first agent; status lines land in the transcript
    // once the TUI starts draining the channel.
    let mcp = mcp::McpServers::new();
    let mut mcp_notices = mcp.reload().await;
    let (plugin_hooks, hook_notices) = mcp.plugin_hook_extension();
    mcp_notices.extend(hook_notices);
    let hook_count = plugin_hooks.as_ref().map_or(0, |hooks| hooks.len());
    for line in super::startup::notices(&mcp, &skills, hook_count, mcp_notices, &skill_notices) {
        let _ = ui_tx.send(UiMsg::Notice(line));
    }
    // The instruction load already ran before the system prompt was built;
    // replay its diagnostics now that there is a transcript.
    for line in instruction_notices {
        let _ = ui_tx.send(UiMsg::Notice(line));
    }
    // Modes that replace human approval say so once up front, and remain
    // visible in the status line for the session.
    match cfg.mode() {
        crate::mode::Mode::Auto => {
            let _ = ui_tx.send(UiMsg::Notice(
                "auto mode · unresolved actions are reviewed automatically".into(),
            ));
        }
        crate::mode::Mode::Yolo => {
            let _ = ui_tx.send(UiMsg::Notice(
                "yolo mode · every gated tool runs without approval prompts".into(),
            ));
        }
        _ => {}
    }
    let build = {
        let cfg = cfg.clone();
        let ui_tx = ui_tx.clone();
        let subagent_depth = subagent_depth.clone();
        let stats = stats.clone();
        let store = store.clone();
        let context_capacity = context_capacity.clone();
        let session = session.clone();
        let mcp = mcp.clone();
        let plugin_hooks = plugin_hooks.clone();
        let skills = skills.clone();
        let mode = mode.clone();
        let todos = todos.clone();
        let files = files.clone();
        let plan_area = plan_area.clone();
        let memory = memory.clone();
        let memory_scope = memory_scope.clone();
        let process_generation = process_generation.clone();
        let subagent_manager = subagent_manager.clone();
        let workflow_store = workflow_store.clone();
        let completions = completions.clone();
        let cmd_tx = cmd_tx.clone();
        move |endpoint: &Endpoint| {
            let generation = process_generation.fetch_add(1, Ordering::AcqRel) + 1;
            let ws = Workspace::new(&cfg.workspace);
            build_agent(
                endpoint.build_model_for_ui(Some(ui_tx.clone())),
                endpoint.provider.label(),
                &endpoint.model,
                subagent_models::choices_with_ui(endpoint, ui_tx.clone()),
                &cfg,
                &ws,
                &ui_tx,
                &subagent_depth,
                &stats,
                &store,
                &context_capacity,
                &session,
                &mcp,
                &plugin_hooks,
                &skills,
                &mode,
                &todos,
                &files,
                &plan_area,
                &memory,
                &memory_scope,
                generation,
                &process_generation,
                &subagent_manager,
                &workflow_store,
                &completions,
                &cmd_tx,
            )
        }
    };
    let agent = build(&endpoint);
    // Everything above is the cold-start path — arg parse, config load,
    // skills scan, system prompt, session open, MCP connect, agent build.
    // Everything below needs a terminal, so `ORCA_BENCH` stops here: it is
    // what lets `benchmarks/startup/run.sh` time the whole startup without a
    // TTY. Nothing else in the binary reads it.
    if std::env::var("ORCA_BENCH").is_ok_and(|v| !v.trim().is_empty() && v != "0") {
        eprintln!("ORCA_BENCH set: exiting after startup, before the terminal UI");
        return ExitCode::SUCCESS;
    }
    // The update check only produces a transcript notice, so it starts
    // with the UI rather than in the cold-start path above: spawned any
    // earlier, the runtime waits on its DNS lookup at shutdown and the
    // startup benchmark measures the network instead of the binary.
    crate::update::check_in_background(ui_tx.clone());
    let initial_provider = endpoint.provider;
    let session_id = session.as_ref().map(|s| s.session_id());
    // The TUI reads per-server tool counts off the same handle the
    // worker reloads; /mcp renders whatever the last reload recorded.
    let tui_mcp = mcp.clone();
    let tui_skills = skills.clone();
    let worker_todos = todos.clone();
    let worker_commands = cmd_tx.clone();
    tokio::spawn(worker(
        agent,
        system,
        endpoint,
        build,
        store,
        context_capacity,
        mcp,
        skills,
        session,
        worker_todos,
        files,
        planning,
        context,
        cmd_rx,
        worker_commands,
        process_generation,
        subagent_manager,
        completions,
        ui_tx,
    ));

    let tui_cfg = tui::TuiConfig {
        model_name: cfg.model.clone(),
        workspace_name: cfg.workspace.display().to_string(),
        workspace_root: workspace_scope(&ws),
        provider: initial_provider,
        subagent_depth,
        stats,
        session_id,
        mcp: tui_mcp,
        skills: tui_skills,
        mode,
        todos,
        plan: plan_area,
    };
    match tui::run(tui_cfg, cmd_tx, ui_rx).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("terminal error: {err}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_agent<M: Model + Clone + 'static>(
    model: M,
    inherited_provider: &str,
    inherited_model: &str,
    subagent_models: Vec<orca_harness_tools::SubagentModel<Arc<dyn Model>>>,
    cfg: &Config,
    ws: &Workspace,
    ui: &mpsc::UnboundedSender<UiMsg>,
    subagent_depth: &SubagentDepth,
    stats: &BackgroundStats,
    store: &TruncationStore,
    context_capacity: &ContextCapacity,
    session: &Option<Arc<SessionHandler>>,
    mcp: &mcp::McpServers,
    plugin_hooks: &Option<Arc<orca_harness_tool_extensions::plugin_hooks::PluginHookExtension>>,
    skills: &skills::Skills,
    mode: &ModeHandle,
    todos: &TodoList,
    files: &FileGuard,
    plan_area: &PlanArea,
    memory: &MemoryStore,
    memory_scope: &MemoryScope,
    process_generation: u64,
    current_process_generation: &Arc<AtomicU64>,
    subagent_manager: &SubagentManager,
    workflow_store: &orca_harness_tools::WorkflowStore,
    completions: &CompletionInbox,
    worker: &mpsc::UnboundedSender<crate::msg::WorkerCmd>,
) -> Agent<Arc<dyn Model>> {
    // MCP visibility is a host-side model concern: core keeps its sacred,
    // immutable schema snapshot while this adapter filters it on each
    // provider request using the catalog's current selections.
    let model: Arc<dyn Model> = Arc::new(McpModel::new(model, mcp.catalog()));
    let model_for_subagents = model.clone();
    let model: Arc<dyn Model> = Arc::new(skills::SkillMentionModel::new(model, skills.clone()));
    let model: Arc<dyn Model> = Arc::new(MemoryModel::new(
        model,
        MemoryExtension::new(memory.clone(), memory_scope.clone()),
    ));
    let auto_approval = AutoApproval::new(mode.clone(), model_for_subagents.clone(), ws.root());
    let events = EventStream::from_fn({
        let ui = ui.clone();
        move |event| {
            let _ = ui.send(UiMsg::Event(event));
        }
    });
    let execution_events = events.execution_marker();
    // PlanGate before Approval: the kernel stops at the first denial, so
    // a call plan mode refuses never reaches the user as a prompt.
    // Both read the shared handle per call: /mode applies to the call
    // in flight, yolo included.
    // Completion delivery sits ahead of session recording and compaction
    // in the before_model chain, so a batch handed over between steps is
    // recorded and budgeted in the same pass.
    let mut agent = Agent::new(model)
        .limits(cfg.limits())
        .extension(events)
        .extension(CompletionDelivery::new(completions.clone(), ui.clone()))
        .extension(PlanGate::new(mode.clone(), plan_area.clone()))
        .extension(orca_harness_tools::MutationPreflight)
        .extension(auto_approval.clone());
    if let Some(plugin_hooks) = plugin_hooks {
        agent = agent.extension_arc(plugin_hooks.clone());
    }
    agent = agent.extension(Approval::with_mode(
        mode.clone(),
        ui.clone(),
        workspace_scope(ws),
    ));
    if extensions::enabled("long-session") {
        let ui = ui.clone();
        agent = agent.extension(
            LongSession::new(context_capacity.clone(), store.clone()).on_compact(move |report| {
                let _ = ui.send(UiMsg::Compacted(Ok(report)));
            }),
        );
    }
    agent = agent.extension(ActiveInventory(subagent_manager.clone()));
    if let Some(session) = session {
        agent = agent.extension_arc(session.clone());
    }
    if extensions::enabled("truncation") {
        agent = agent.extension(Truncation::new(16_000).store(store.clone()));
    }
    if extensions::enabled("retry") {
        agent = agent.extension(extensions::tool_retry());
    }
    // Last in the around-tool chain: everything before this point is host
    // preflight, while everything after it is actual execution.
    agent = agent.extension(execution_events);
    // After LongSession: it reads "already loaded" off the context the
    // model is about to see, which compaction may have just shrunk.
    agent = agent.extension(SkillOnce::new());
    // read_tool_result stays registered even with truncation off so
    // outputs trimmed before the toggle remain pageable.
    agent = agent
        .tool_arc(std::sync::Arc::new(ReadToolResultTool::new(store.clone())))
        .tool_arc(std::sync::Arc::new(TodoWriteTool::new(todos.clone())))
        .tool_arc(std::sync::Arc::new(MemorySearchTool::new(
            memory.clone(),
            memory_scope.clone(),
        )))
        .tool_arc(std::sync::Arc::new(MemoryManageTool::new(
            memory.clone(),
            memory_scope.clone(),
        )))
        .tool_arc(std::sync::Arc::new(AskTool::new({
            let ui = ui.clone();
            move |request| match ui.send(UiMsg::Ask(request)) {
                Ok(()) => Ok(()),
                Err(err) => match err.0 {
                    UiMsg::Ask(request) => Err(request),
                    _ => unreachable!("ask callback only sends UiMsg::Ask"),
                },
            }
        })))
        .tool_arc(std::sync::Arc::new(WebFetchTool::new(UrlPolicy::strict())));
    if let Some(key) = &cfg.firecrawl_key {
        let fc = std::sync::Arc::new(Firecrawl::new(key.clone()));
        agent = agent
            .tool_arc(std::sync::Arc::new(WebSearchTool::new(fc.clone())))
            .tool_arc(std::sync::Arc::new(WebCrawlTool::new(fc)));
    }
    for tool in mcp.tools() {
        agent = agent.tool_arc(tool);
    }
    // One `skill` tool carrying the whole catalog, or none at all when
    // nothing was found or everything is switched off.
    if let Some(tool) = skills.tool() {
        agent = agent.tool_arc(tool);
    }
    // One guard for the whole session: the agent is rebuilt on every
    // model switch and config reload, and what the model has read must
    // not be forgotten each time.
    for tool in core_tools_with_guard(ws, files) {
        agent = agent.tool_arc(tool);
    }
    let root = ws.root().to_string_lossy().into_owned();
    // Re-registering `process` replaces core_tools' entry by name (its
    // position is kept) so it can carry the shared stats handle.
    agent = agent.tool_arc(std::sync::Arc::new(
        ProcessTool::local()
            .working_dir(root.clone())
            .stats(stats.clone())
            .on_notification({
                let worker = worker.clone();
                let current = current_process_generation.clone();
                let sequence = Arc::new(AtomicU64::new(0));
                move |notification| {
                    if current.load(Ordering::Acquire) == process_generation {
                        let _ = worker.send(crate::msg::WorkerCmd::BackgroundProcess {
                            generation: process_generation,
                            sequence: sequence.fetch_add(1, Ordering::Relaxed) + 1,
                            notification,
                        });
                    }
                }
            }),
    ));
    agent = agent.tool_arc(std::sync::Arc::new(
        PyKernelTool::new()
            .working_dir(root.clone())
            .stats(stats.clone()),
    ));
    agent = agent.tool_arc(std::sync::Arc::new(
        BunReplTool::new().working_dir(root).stats(stats.clone()),
    ));
    let ui_events = ui.clone();
    let mut subagent = super::subagents::tool(
        model_for_subagents,
        ws,
        (inherited_provider, inherited_model),
        subagent_depth,
        subagent_models,
        mcp.catalog(),
    )
    .stats(stats.clone())
    .background(subagent_manager.clone(), {
        let worker = worker.clone();
        let completions = completions.clone();
        let ui = ui.clone();
        move |notification| completions.publish(notification, &ui, &worker)
    });
    let subagent_mode = mode.clone();
    let subagent_plan = plan_area.clone();
    let subagent_settings = subagent_depth.clone();
    let subagent_plugin_hooks = plugin_hooks.clone();
    let subagent_auto_approval = auto_approval.for_subagent();
    subagent = subagent.spawn_extensions(std::sync::Arc::new(move |spawn: &SubagentSpawn| {
        subagent_extensions(
            spawn,
            &ui_events,
            &subagent_mode,
            &subagent_plan,
            &subagent_settings,
            subagent_plugin_hooks.as_ref(),
            Some(&subagent_auto_approval),
        )
    }));
    let subagent = std::sync::Arc::new(subagent);
    agent = agent.tool_arc(std::sync::Arc::new(
        orca_harness_tools::WorkflowTool::new(subagent.clone(), workflow_store.clone())
            .expect("interactive host provides depth-zero background execution"),
    ));
    agent = agent.tool_arc(subagent);
    agent
}
