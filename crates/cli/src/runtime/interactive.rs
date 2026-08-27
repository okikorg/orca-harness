use super::agent::subagent_extensions;
use super::session::open_session;
use super::worker::worker;
use std::process::ExitCode;
use std::sync::Arc;

use tokio::sync::mpsc;

use orca_harness_core::{Agent, Context, Model};
use orca_harness_extensions::{
    workspace_key, ContextCapacity, EventStream, LongSession, MemoryExtension, MemoryManageTool,
    MemoryModel, MemoryScope, MemorySearchTool, MemoryStore, ReadToolResultTool, SessionHandler,
    Truncation, TruncationStore,
};
use orca_harness_tool_extensions::mcp::McpModel;
use orca_harness_tool_extensions::web::{
    Firecrawl, UrlPolicy, WebCrawlTool, WebFetchTool, WebSearchTool,
};
use orca_harness_tools::{
    core_tools_with_guard, AskTool, BackgroundStats, BunReplTool, FileGuard, ProcessTool,
    PyKernelTool, SubagentDepth, SubagentSpawn, SubagentTool, TodoList, TodoWriteTool, Workspace,
};

use crate::approval::Approval;
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
pub(crate) async fn run_mode(cfg: Config) -> ExitCode {
    let ws = Workspace::new(&cfg.workspace);
    // Scanned before the prompt is built: whether the `skill` tool gets
    // advertised depends on whether any skill was found, and the scan is
    // a handful of read_dir calls.
    let skills = skills::Skills::for_session(&cfg.workspace);
    let skill_notices = skills.reload();
    // The base prompt is a fixed string with tests asserting its
    // contents; the user's standing instructions are appended here, at
    // the call site, so no AGENTS.md can ever change what that function
    // returns. `/clear` re-pushes this composed string, so instructions
    // survive a reset.
    let instructions = instructions::Instructions::load(&cfg.workspace);
    let instruction_notices = instructions.notices();
    let mut system = system_prompt(&ws, cfg.firecrawl_key.is_some());
    if let Some(block) = instructions.block() {
        system.push_str(&block);
    }
    let endpoint = Endpoint::from_config(&cfg);
    let subagent_depth = SubagentDepth::new(cfg.subagent_depth);
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

    let session = if cfg.no_session {
        None
    } else {
        match open_session(&cfg, &ws) {
            Ok(opened) => Some(opened),
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        }
    };

    if cfg.prompt.is_some() {
        let (handler, resumed) = match session {
            Some((handler, resumed)) => (Some(Arc::new(handler)), resumed),
            None => (None, None),
        };
        for line in skill_notices.iter().chain(instruction_notices.iter()) {
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
            &mode,
            &todos,
            &plan_area,
            &memory,
            &memory_scope,
        )
        .await;
        return ExitCode::from(code as u8);
    }

    // Interactive: worker task owns the agent; UI owns the terminal.
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

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
    for line in mcp.reload().await {
        let _ = ui_tx.send(UiMsg::Notice(line));
    }
    // The skills scan and the instruction load already ran (the system
    // prompt depended on both); replay what they had to say now that
    // there is a transcript.
    for line in skill_notices.into_iter().chain(instruction_notices) {
        let _ = ui_tx.send(UiMsg::Notice(line));
    }
    // A session that starts in yolo says so once, up front. The status
    // line keeps saying it for the rest of the session; this is the
    // prose version, so the first thing on the transcript is honest
    // about what will (not) be asked.
    if cfg.mode().bypasses_approval() {
        let _ = ui_tx.send(UiMsg::Notice(
            "yolo mode · every gated tool runs without approval prompts".into(),
        ));
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
        let skills = skills.clone();
        let mode = mode.clone();
        let todos = todos.clone();
        let files = files.clone();
        let plan_area = plan_area.clone();
        let memory = memory.clone();
        let memory_scope = memory_scope.clone();
        move |endpoint: &Endpoint| {
            let ws = Workspace::new(&cfg.workspace);
            build_agent(
                endpoint.build_model_for_ui(Some(ui_tx.clone())),
                endpoint.provider.label(),
                &endpoint.model,
                subagent_models::choices(endpoint),
                &cfg,
                &ws,
                &ui_tx,
                &subagent_depth,
                &stats,
                &store,
                &context_capacity,
                &session,
                &mcp,
                &skills,
                &mode,
                &todos,
                &files,
                &plan_area,
                &memory,
                &memory_scope,
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
    skills: &skills::Skills,
    mode: &ModeHandle,
    todos: &TodoList,
    files: &FileGuard,
    plan_area: &PlanArea,
    memory: &MemoryStore,
    memory_scope: &MemoryScope,
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
    let events = EventStream::from_fn({
        let ui = ui.clone();
        move |event| {
            let _ = ui.send(UiMsg::Event(event));
        }
    });
    // PlanGate before Approval: the kernel stops at the first denial, so
    // a call plan mode refuses never reaches the user as a prompt.
    // Both read the shared handle per call: /mode applies to the call
    // in flight, yolo included.
    let mut agent = Agent::new(model)
        .limits(cfg.limits())
        .extension(events)
        .extension(PlanGate::new(mode.clone(), plan_area.clone()))
        .extension(Approval::with_mode(
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
    if let Some(session) = session {
        agent = agent.extension_arc(session.clone());
    }
    if extensions::enabled("truncation") {
        agent = agent.extension(Truncation::new(16_000).store(store.clone()));
    }
    if extensions::enabled("retry") {
        agent = agent.extension(extensions::tool_retry());
    }
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
            .stats(stats.clone()),
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
    let mut subagent = SubagentTool::new(model_for_subagents, ws)
        .inherited_identity(inherited_provider, inherited_model)
        .models(
            subagent_models
                .into_iter()
                .map(|choice| orca_harness_tools::SubagentModel {
                    model: Arc::new(McpModel::new(choice.model, mcp.catalog())) as Arc<dyn Model>,
                    ..choice
                }),
        )
        .max_depth(subagent_depth.clone())
        .stats(stats.clone());
    if extensions::enabled("retry") {
        // Install defaults once; subsequent rebuilds preserve `/subagents`
        // choices while attaching the same data-failure classifier.
        subagent_depth.ensure_retry_defaults(3, 250);
        subagent = subagent.retry_ok_when(extensions::data_failure);
    }
    let subagent_mode = mode.clone();
    let subagent_plan = plan_area.clone();
    let subagent_settings = subagent_depth.clone();
    subagent = subagent.spawn_extensions(std::sync::Arc::new(move |spawn: &SubagentSpawn| {
        subagent_extensions(
            spawn,
            &ui_events,
            &subagent_mode,
            &subagent_plan,
            &subagent_settings,
        )
    }));
    agent = agent.tool_arc(std::sync::Arc::new(subagent));
    agent
}
