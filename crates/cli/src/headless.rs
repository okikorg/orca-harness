//! Headless single-shot mode: `orcacode -p "prompt"`. Streams assistant text
//! to stdout as it is generated; tool activity goes to stderr. With
//! `--json`, every harness event is serialized to stdout as NDJSON.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use orca_harness_core::{Agent, CancellationToken, Context, Model};
use orca_harness_extensions::{
    EventStream, HarnessEvent, MemoryExtension, MemoryManageTool, MemoryModel, MemoryScope,
    MemorySearchTool, MemoryStore, Truncation, UsageMeter,
};
use orca_harness_tool_extensions::mcp::McpModel;
use orca_harness_tool_extensions::web::{
    Firecrawl, UrlPolicy, WebCrawlTool, WebFetchTool, WebSearchTool,
};
use orca_harness_tools::{
    core_tools, BunReplTool, PyKernelTool, SubagentDepth, SubagentModel, SubagentTool, TodoList,
    TodoWriteTool, Workspace,
};

use crate::approval::HeadlessGate;
use crate::mode::{ModeHandle, PlanGate};
use crate::presentation;
use crate::Config;

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
    mode: &ModeHandle,
    todos: &TodoList,
    plan_area: &crate::plan::PlanArea,
    memory: &MemoryStore,
    memory_scope: &MemoryScope,
) -> i32 {
    // Connect before constructing the model adapter so selection made by an
    // MCP tool is reflected in the immediately following provider request.
    let mcp = crate::mcp::McpServers::new();
    for line in mcp.reload().await {
        eprintln!("{line}");
    }
    let model: Arc<dyn Model> = Arc::new(McpModel::new(model, mcp.catalog()));
    let model_for_subagents = model.clone();
    let model: Arc<dyn Model> =
        Arc::new(crate::skills::SkillMentionModel::new(model, skills.clone()));
    let model: Arc<dyn Model> = Arc::new(MemoryModel::new(
        model,
        MemoryExtension::new(memory.clone(), memory_scope.clone()),
    ));
    let json = cfg.json;
    let saw_delta = Arc::new(AtomicBool::new(false));
    let saw = saw_delta.clone();
    let events = EventStream::from_fn(move |ev: HarnessEvent| {
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

    let (meter, usage) = UsageMeter::new();
    // PlanGate first: --plan denies before --yolo/--auto-approve can allow.
    let mut agent = Agent::new(model)
        .limits(cfg.limits())
        .extension(events)
        .extension(meter)
        .extension(PlanGate::new(mode.clone(), plan_area.clone()));
    if crate::extensions::enabled("truncation") {
        agent = agent.extension(Truncation::new(16_000));
    }
    if crate::extensions::enabled("retry") {
        agent = agent.extension(crate::extensions::tool_retry());
    }
    // Yolo implies auto-approve: a headless run started with --yolo has
    // opted out of every ask, so the headless gate must not re-add the
    // one the interactive path just removed.
    if !cfg.auto_approve && !cfg.mode().bypasses_approval() {
        agent = agent.extension(HeadlessGate);
    }
    if let Some(session) = &session {
        agent = agent.extension_arc(session.clone());
    }
    for tool in core_tools(ws) {
        agent = agent.tool_arc(tool);
    }
    agent = agent
        .tool_arc(Arc::new(TodoWriteTool::new(todos.clone())))
        .tool_arc(Arc::new(MemorySearchTool::new(
            memory.clone(),
            memory_scope.clone(),
        )))
        .tool_arc(Arc::new(MemoryManageTool::new(
            memory.clone(),
            memory_scope.clone(),
        )))
        .tool_arc(Arc::new(WebFetchTool::new(UrlPolicy::strict())));
    if let Some(key) = &cfg.firecrawl_key {
        let firecrawl = Arc::new(Firecrawl::new(key.clone()));
        agent = agent
            .tool_arc(Arc::new(WebSearchTool::new(firecrawl.clone())))
            .tool_arc(Arc::new(WebCrawlTool::new(firecrawl)));
    }
    let root = ws.root().to_string_lossy().into_owned();
    agent = agent.tool_arc(Arc::new(PyKernelTool::new().working_dir(root.clone())));
    agent = agent.tool_arc(Arc::new(BunReplTool::new().working_dir(root)));
    let subagent_settings = SubagentDepth::new(cfg.subagent_depth);
    let extension_settings = subagent_settings.clone();
    let subagent = SubagentTool::new(model_for_subagents, ws)
        .inherited_identity(cfg.provider.label(), &cfg.model)
        .max_depth(subagent_settings.clone())
        .models(subagent_models.into_iter().map(|choice| SubagentModel {
            model: Arc::new(McpModel::new(choice.model, mcp.catalog())) as Arc<dyn Model>,
            ..choice
        }));
    crate::config::load_subagent_settings(&subagent_settings);
    agent = agent.tool_arc(Arc::new(subagent.spawn_extensions(Arc::new(move |_| {
        vec![
            Arc::new(Truncation::new(extension_settings.output_chars() as usize))
                as Arc<dyn orca_harness_core::Extension>,
        ]
    }))));
    // Configured MCP servers joined before model construction; register the
    // stable interfaces and hidden remote dispatch targets here.
    for tool in mcp.tools() {
        agent = agent.tool_arc(tool);
    }
    // Skills join headless runs on the same terms: the caller scanned
    // before building the system prompt and reported anything broken.
    if let Some(tool) = skills.tool() {
        agent = agent.tool_arc(tool);
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

    let result = agent.run_context(&mut context, cancel).await;
    if !json {
        println!();
        let totals = usage.total();
        eprintln!(
            "tokens: {} in, {} out",
            totals.input_tokens, totals.output_tokens
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
