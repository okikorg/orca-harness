//! Headless single-shot mode: `orcacode -p "prompt"`. Streams assistant text
//! to stdout as it is generated; tool activity goes to stderr. With
//! `--json`, every harness event is serialized to stdout as NDJSON.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use orca_harness_core::{Agent, CancellationToken, Context, Model};
use orca_harness_extensions::{EventStream, HarnessEvent, Truncation, UsageMeter};
use orca_harness_tools::{core_tools, PyKernelTool, SubagentDepth, SubagentTool, Workspace};

use crate::approval::HeadlessGate;
use crate::view;
use crate::Config;

pub async fn run<M: Model + Clone + 'static>(
    cfg: &Config,
    model: M,
    ws: &Workspace,
    system_prompt: &str,
    session: Option<Arc<orca_harness_extensions::SessionHandler>>,
    resumed: Option<Context>,
) -> i32 {
    let model_for_subagents = model.clone();
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
                eprintln!("• {}", view::tool_call_line(tool_name, input));
            }
            HarnessEvent::ToolResult {
                tool_name,
                output,
                is_error,
                ..
            } => {
                eprintln!(
                    "  {}",
                    view::tool_result_summary(tool_name, output, *is_error)
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
    let mut agent = Agent::new(model)
        .limits(cfg.limits())
        .extension(events)
        .extension(meter);
    if crate::extensions::enabled("truncation") {
        agent = agent.extension(Truncation::new(16_000));
    }
    if crate::extensions::enabled("retry") {
        agent = agent.extension(crate::extensions::tool_retry());
    }
    if !cfg.auto_approve {
        agent = agent.extension(HeadlessGate);
    }
    if let Some(session) = &session {
        agent = agent.extension_arc(session.clone());
    }
    for tool in core_tools(ws) {
        agent = agent.tool_arc(tool);
    }
    let root = ws.root().to_string_lossy().into_owned();
    agent = agent.tool_arc(Arc::new(PyKernelTool::new().working_dir(root)));
    agent = agent.tool_arc(Arc::new(
        SubagentTool::new(model_for_subagents, ws)
            .max_depth(SubagentDepth::new(cfg.subagent_depth)),
    ));

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
    context.push_user(cfg.prompt.as_deref().unwrap_or_default());

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
