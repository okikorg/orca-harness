//! Headless single-shot mode: `orcacode -p "prompt"`. Streams assistant text
//! to stdout as it is generated; tool activity goes to stderr. With
//! `--json`, every harness event is serialized to stdout as NDJSON.

use std::collections::HashSet;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use orca_harness_core::{Agent, CancellationToken, Context, Model};
use orca_harness_extensions::{
    EventStream, HarnessEvent, MemoryExtension, MemoryManageTool, MemoryModel, MemoryScope,
    MemorySearchTool, MemoryStore, Truncation, UsageMeter,
};
use orca_harness_tool_extensions::mcp::McpModel;
use orca_harness_tool_extensions::skills::SkillOnce;
use orca_harness_tool_extensions::web::{
    Firecrawl, UrlPolicy, WebCrawlTool, WebFetchTool, WebSearchTool,
};
use orca_harness_tools::{
    core_tools, BunReplTool, PyKernelTool, SubagentDepth, SubagentModel, TodoList, TodoWriteTool,
    Workspace,
};

use crate::approval::HeadlessGate;
use crate::auto_approval::AutoApproval;
use crate::mode::{ModeHandle, PlanGate};
use crate::presentation;
use crate::Config;

const BARE_TOOLS: [&str; 4] = ["read_file", "list_dir", "grep", "glob"];

pub(crate) fn bare_system_prompt(ws: &Workspace, tools: &[String]) -> String {
    format!(
        "You are Orca Code, a coding agent operating in the workspace at {} on {}. \
         Use only the registered tools: {}. File paths are workspace-relative. \
         Investigate with tools instead of guessing. When the request names files, read \
         them directly; otherwise search workspace-wide before narrowing to any one \
         subdirectory. Batch independent reads and stop as soon as the evidence answers \
         the question — with one exception: before reporting that something is absent, \
         missing, or does not match, widen the search once (a different term, the whole \
         workspace, the files a listing already showed you) and then answer either way. \
         Your final response is machine parsed. If the user specifies an exact final \
         line, the entire response must be only that line: no analysis, prose, markdown, \
         code fence, or added punctuation, and no trailing punctuation carried in from \
         text you are quoting.",
        ws.root().display(),
        std::env::consts::OS,
        tools.join(", ")
    )
}

pub(crate) fn selected_tool_names(cfg: &Config) -> Option<Vec<String>> {
    cfg.tools.clone().or_else(|| {
        cfg.bare
            .then(|| BARE_TOOLS.iter().map(|name| (*name).to_string()).collect())
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn run<M: Model + Clone + 'static>(
    cfg: &Config,
    model: M,
    subagent_models: Vec<SubagentModel<Arc<dyn Model>>>,
    ws: &Workspace,
    system_prompt: &str,
    session: Option<Arc<orca_harness_extensions::SessionHandler>>,
    resumed: Option<Context>,
    skills: &crate::skills::Skills,
    skill_notices: &[String],
    mode: &ModeHandle,
    todos: &TodoList,
    plan_area: &crate::plan::PlanArea,
    memory: &MemoryStore,
    memory_scope: &MemoryScope,
    model_retries: Arc<AtomicU64>,
    subagent_settings: SubagentDepth,
) -> i32 {
    // Connect before constructing the model adapter so selection made by an
    // MCP tool is reflected in the immediately following provider request.
    let mcp = crate::mcp::McpServers::new();
    let mut plugin_hooks = None;
    if !cfg.bare {
        let mut mcp_notices = mcp.reload().await;
        let (hooks, hook_notices) = mcp.plugin_hook_extension();
        mcp_notices.extend(hook_notices);
        let hook_count = hooks.as_ref().map_or(0, |hooks| hooks.len());
        plugin_hooks = hooks;
        for line in
            crate::runtime::startup::notices(&mcp, skills, hook_count, mcp_notices, skill_notices)
        {
            eprintln!("{line}");
        }
    }
    let model: Arc<dyn Model> = Arc::new(model);
    let model: Arc<dyn Model> = if cfg.bare {
        model
    } else {
        Arc::new(McpModel::new(model, mcp.catalog()))
    };
    let model_for_subagents = model.clone();
    let model: Arc<dyn Model> = if cfg.bare {
        model
    } else {
        Arc::new(crate::skills::SkillMentionModel::new(model, skills.clone()))
    };
    let model: Arc<dyn Model> = if cfg.bare {
        model
    } else {
        Arc::new(MemoryModel::new(
            model,
            MemoryExtension::new(memory.clone(), memory_scope.clone()),
        ))
    };
    let json = cfg.json;
    let selected_names = selected_tool_names(cfg);
    let selected = selected_names
        .as_ref()
        .map(|names| names.iter().map(String::as_str).collect::<HashSet<_>>());
    let enabled = |name: &str| selected.as_ref().is_none_or(|names| names.contains(name));
    let tool_calls = Arc::new(AtomicU64::new(0));
    let counted_tool_calls = tool_calls.clone();
    let saw_delta = Arc::new(AtomicBool::new(false));
    let saw = saw_delta.clone();
    let events = EventStream::from_fn(move |ev: HarnessEvent| {
        if matches!(ev, HarnessEvent::ToolCall { .. }) {
            counted_tool_calls.fetch_add(1, Ordering::Relaxed);
        }
        if json {
            if let Ok(line) = serde_json::to_string(&ev) {
                println!("{line}");
            }
            return;
        }
        match &ev {
            HarnessEvent::AssistantDelta { text } => {
                saw.store(true, Ordering::Relaxed);
                print!("{text}");
                std::io::stdout().flush().ok();
            }
            HarnessEvent::ReasoningDelta { text } => {
                eprint!("{text}");
                std::io::stderr().flush().ok();
            }
            HarnessEvent::ToolCall {
                tool_name, input, ..
            } => {
                eprintln!("• {}", presentation::tool_call_line(tool_name, input));
            }
            HarnessEvent::ToolResult {
                tool_name,
                output,
                is_error,
                ..
            } => {
                eprintln!(
                    "  {}",
                    presentation::tool_result_summary(tool_name, output, *is_error)
                );
            }
            HarnessEvent::Result { message } => {
                if !saw.load(Ordering::Relaxed) && !message.is_empty() {
                    print!("{message}");
                    std::io::stdout().flush().ok();
                }
            }
            _ => {}
        }
    });
    let execution_events = events.execution_marker();

    // Only the parent receives the live orchestration briefing, not workers/reviewers.
    let model: Arc<dyn Model> = Arc::new(crate::mode::OrchestrateModel::new(model, mode.clone()));
    let auto_approval = AutoApproval::new(mode.clone(), model_for_subagents.clone(), ws.root());
    let (meter, usage) = UsageMeter::new();
    // PlanGate first: --plan denies before --yolo/--auto-approve can allow.
    let mut agent = Agent::new(model)
        .limits(cfg.limits())
        .extension(events)
        .extension(meter)
        .extension(PlanGate::new(mode.clone(), plan_area.clone()))
        .extension(orca_harness_tools::MutationPreflight)
        .extension(auto_approval.clone());
    if let Some(plugin_hooks) = &plugin_hooks {
        agent = agent.extension_arc(plugin_hooks.clone());
    }
    if crate::extensions::enabled("truncation") {
        agent = agent.extension(Truncation::new(16_000));
    }
    if crate::extensions::enabled("retry") {
        agent = agent.extension(crate::extensions::tool_retry());
    }
    // Auto owns unresolved admission and yolo disables it, so neither may
    // have the headless human gate re-added afterward.
    if !cfg.auto_approve && !cfg.mode().bypasses_human_approval() {
        agent = agent.extension(HeadlessGate);
    }
    if let Some(session) = &session {
        agent = agent.extension_arc(session.clone());
    }
    agent = agent.extension(execution_events);
    if !cfg.bare && enabled("skill") {
        agent = agent.extension(SkillOnce::new());
    }
    for tool in core_tools(ws) {
        if enabled(&tool.schema().name) {
            agent = agent.tool_arc(tool);
        }
    }
    if !cfg.bare && enabled("todo_write") {
        agent = agent.tool_arc(Arc::new(TodoWriteTool::new(todos.clone())));
    }
    if !cfg.bare && enabled("memory_search") {
        agent = agent.tool_arc(Arc::new(MemorySearchTool::new(
            memory.clone(),
            memory_scope.clone(),
        )));
    }
    if !cfg.bare && enabled("memory_manage") {
        agent = agent.tool_arc(Arc::new(MemoryManageTool::new(
            memory.clone(),
            memory_scope.clone(),
        )));
    }
    if !cfg.bare && enabled("web_fetch") {
        agent = agent.tool_arc(Arc::new(WebFetchTool::new(UrlPolicy::strict())));
    }
    if !cfg.bare {
        if let Some(key) = &cfg.firecrawl_key {
            let firecrawl = Arc::new(Firecrawl::new(key.clone()));
            if enabled("web_search") {
                agent = agent.tool_arc(Arc::new(WebSearchTool::new(firecrawl.clone())));
            }
            if enabled("web_crawl") {
                agent = agent.tool_arc(Arc::new(WebCrawlTool::new(firecrawl)));
            }
        }
    }
    if !cfg.bare {
        let root = ws.root().to_string_lossy().into_owned();
        if enabled("pykernel") {
            agent = agent.tool_arc(Arc::new(PyKernelTool::new().working_dir(root.clone())));
        }
        if enabled("bun_repl") {
            agent = agent.tool_arc(Arc::new(BunReplTool::new().working_dir(root)));
        }
        if enabled("subagent") {
            let extension_settings = subagent_settings.clone();
            let subagent = crate::runtime::subagents::tool(
                model_for_subagents,
                ws,
                (cfg.provider.label(), &cfg.model),
                &subagent_settings,
                subagent_models,
                mcp.catalog(),
            );
            let subagent_plugin_hooks = plugin_hooks.clone();
            let subagent_auto_approval = auto_approval.for_subagent();
            agent = agent.tool_arc(Arc::new(subagent.spawn_extensions(Arc::new(move |_| {
                let host_hooks = [
                    Some(Arc::new(subagent_auto_approval.clone())
                        as Arc<dyn orca_harness_core::Extension>),
                    subagent_plugin_hooks
                        .as_ref()
                        .map(|hooks| hooks.clone() as Arc<dyn orca_harness_core::Extension>),
                ];
                crate::runtime::subagents::extensions(
                    &extension_settings,
                    host_hooks.into_iter().flatten(),
                )
            }))));
        }
    }
    // Configured MCP servers joined before model construction; register the
    // stable interfaces and hidden remote dispatch targets here.
    if !cfg.bare {
        for tool in mcp.tools() {
            if enabled(&tool.schema().name) {
                agent = agent.tool_arc(tool);
            }
        }
    }
    // Skills join headless runs on the same terms: the caller scanned
    // before building the system prompt and reported anything broken.
    if !cfg.bare && enabled("skill") {
        if let Some(tool) = skills.tool() {
            agent = agent.tool_arc(tool);
        }
    }

    let cancel = CancellationToken::new();
    let cancel_on_signal = cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if result.is_err() {
                    return;
                }
            }
            _ = crate::shutdown_signal() => {}
        }
        cancel_on_signal.cancel();
    });

    let mut context = match resumed {
        // A recorded transcript already begins with its system prompt.
        Some(context) => context,
        None => {
            let mut context = Context::new();
            context.push_system(system_prompt);
            context
        }
    };
    let prompt = cfg.prompt.as_deref().unwrap_or_default();
    // `--plan` briefs the model the same way the interactive worker
    // does: the rules, the writable directory, and today's date.
    // Whether a plan file appears is the agent's call. Without
    // --auto-approve the write is still refused by HeadlessGate —
    // there is nobody to ask.
    if mode.is_plan() && plan_area.open() {
        context.push_system(crate::plan::briefing(&crate::plan::today()));
    }
    context.push_user(crate::prompt::strip_location_mentions(prompt));

    let started = Instant::now();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "type": "metadata",
                "provider": cfg.provider.label(),
                "model": cfg.model,
                "effort": cfg.reasoning_effort,
                "promptCache": cfg.prompt_cache,
                "maxOutputTokens": cfg.max_output_tokens,
                "bare": cfg.bare,
                "tools": selected_names,
            })
        );
    }
    let result = agent.run_context(&mut context, cancel).await;
    let totals = usage.total();
    if !json {
        println!();
        eprintln!(
            "tokens: {} in, {} out",
            totals.input_tokens, totals.output_tokens
        );
    }
    let status = if result.is_ok() { "success" } else { "error" };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "type": "summary",
                "status": status,
                "provider": cfg.provider.label(),
                "model": cfg.model,
                "effort": cfg.reasoning_effort,
                "durationMs": started.elapsed().as_millis(),
                "usage": totals,
                "modelSteps": usage.metered_steps(),
                "toolCalls": tool_calls.load(Ordering::Relaxed),
                "modelRetries": model_retries.load(Ordering::Relaxed),
            })
        );
    }
    match result {
        Ok(_) => 0,
        Err(err) => {
            eprintln!("error: {err}");
            1
        }
    }
}
