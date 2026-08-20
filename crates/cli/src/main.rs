//! `orca` — the interactive terminal host for Orca Harness.
//!
//! Interactive: `orca` starts a streaming REPL with tool approvals.
//! Headless:    `orca -p "prompt"` runs once and streams to stdout.

mod approval;
mod commands;
mod headless;
mod msg;
mod tui;
mod view;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use tokio::sync::mpsc;

use orca_harness_core::{Agent, Context, Limits, Message, Model, ToolResult};
use orca_harness_extensions::{EventStream, Truncation};
use orca_harness_model_openai::OpenAiModel;
use orca_harness_model_openrouter::{self as openrouter, OpenRouterModel};
use orca_harness_tools::{core_tools, Workspace};

use crate::approval::Approval;
use crate::msg::{Provider, UiMsg, WorkerCmd};

const USAGE: &str = "\
orca — terminal host for Orca Harness

USAGE:
  orca [OPTIONS]                interactive session
  orca [OPTIONS] -p \"prompt\"    headless single run (streams to stdout)

OPTIONS:
  --model NAME       model id (env ORCA_MODEL; default qwen3.5:9b,
                     or openrouter/auto with --openrouter)
  --base-url URL     OpenAI-compatible endpoint (env ORCA_BASE_URL;
                     default: api.openai.com if OPENAI_API_KEY is set,
                     otherwise http://localhost:11434/v1)
  --api-key KEY      bearer token (env OPENAI_API_KEY, or
                     OPENROUTER_API_KEY with --openrouter)
  --openrouter       use OpenRouter (openrouter.ai) as the endpoint
  --list-models      print the endpoint's model catalog and exit
  --workspace DIR    tool workspace root (default: current directory)
  --max-steps N      model invocations per run (default 48)
  --theme NAME       mono (default) or color
  --json             headless: emit NDJSON harness events on stdout
  --auto-approve     headless: allow shell/write/edit without approval
  -p, --prompt TEXT  headless prompt
  -h, --help         show this help

In the TUI, /models [filter] lists the catalog and /model <id> switches.
";

#[derive(Clone)]
pub struct Config {
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub openrouter: bool,
    pub list_models: bool,
    pub workspace: PathBuf,
    pub prompt: Option<String>,
    pub json: bool,
    pub auto_approve: bool,
    pub max_steps: u32,
    pub theme: String,
}

impl Config {
    pub fn limits(&self) -> Limits {
        Limits {
            max_steps: self.max_steps,
            ..Limits::default()
        }
    }
}

fn parse_args() -> Result<Config, String> {
    let mut model = std::env::var("ORCA_MODEL").ok();
    let mut base_url = std::env::var("ORCA_BASE_URL").ok();
    let mut api_key: Option<String> = None;
    let mut openrouter = false;
    let mut list_models = false;
    let mut workspace = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut prompt = None;
    let mut json = false;
    let mut auto_approve = false;
    let mut max_steps = 48;
    let mut theme = std::env::var("ORCA_THEME").unwrap_or_else(|_| "mono".into());

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match arg.as_str() {
            "--model" => model = Some(value("--model")?),
            "--base-url" => base_url = Some(value("--base-url")?),
            "--api-key" => api_key = Some(value("--api-key")?),
            "--openrouter" => openrouter = true,
            "--list-models" => list_models = true,
            "--workspace" => workspace = PathBuf::from(value("--workspace")?),
            "--max-steps" => {
                max_steps = value("--max-steps")?
                    .parse()
                    .map_err(|_| "--max-steps expects a number".to_string())?
            }
            "--theme" => theme = value("--theme")?,
            "--json" => json = true,
            "--auto-approve" => auto_approve = true,
            "-p" | "--prompt" => prompt = Some(value("-p")?),
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag: {other}\n\n{USAGE}")),
        }
    }

    // An explicit --api-key wins; otherwise each endpoint has its own env.
    let api_key = api_key.or_else(|| {
        let env = if openrouter {
            "OPENROUTER_API_KEY"
        } else {
            "OPENAI_API_KEY"
        };
        std::env::var(env).ok()
    });
    let base_url = base_url.unwrap_or_else(|| {
        if openrouter {
            openrouter::OPENROUTER_BASE_URL.into()
        } else if api_key.is_some() {
            "https://api.openai.com/v1".into()
        } else {
            "http://localhost:11434/v1".into()
        }
    });
    let model = model.unwrap_or_else(|| {
        if openrouter {
            // OpenRouter's auto-router: always a valid id.
            "openrouter/auto".into()
        } else {
            "qwen3.5:9b".into()
        }
    });

    Ok(Config {
        model,
        base_url,
        api_key,
        openrouter,
        list_models,
        workspace,
        prompt,
        json,
        auto_approve,
        max_steps,
        theme,
    })
}

fn system_prompt(ws: &Workspace) -> String {
    format!(
        "You are Orca, a coding agent operating in the workspace at {root} on {os}. \
         You act through tools: shell, read_file, write_file, edit_file, list_dir, grep. \
         File paths are workspace-relative. Investigate with tools instead of guessing; \
         run commands to verify your work. Keep responses brief and concrete: report \
         what you did and what you found.\n\
         \n\
         Tool calls issued in the same response execute concurrently. Before acting, \
         plan the batch: decide everything you can learn or do right now that does not \
         depend on another call's result, and issue all of those calls together in one \
         response — reading several files, running independent searches, or executing \
         unrelated commands should be one batch, not a sequence of turns. Serialize \
         only when a call's input genuinely requires another call's output. Writes to \
         the same file are ordered for you; unrelated writes are safe to batch.",
        root = ws.root().display(),
        os = std::env::consts::OS,
    )
}

/// The worker's current endpoint: which provider, where, with what key,
/// and which model id is active. Type-erasing the adapter behind
/// `Arc<dyn Model>` is what lets one session switch providers.
struct Endpoint {
    provider: Provider,
    base_url: String,
    api_key: Option<String>,
    model: String,
}

impl Endpoint {
    fn from_config(cfg: &Config) -> Self {
        let provider = if cfg.openrouter || cfg.base_url.contains("openrouter.ai") {
            Provider::OpenRouter
        } else if cfg.base_url.contains("api.openai.com") {
            Provider::OpenAi
        } else {
            Provider::Local
        };
        Self {
            provider,
            base_url: cfg.base_url.clone(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
        }
    }

    fn build_model(&self) -> Arc<dyn Model> {
        match self.provider {
            Provider::OpenRouter => {
                let mut model = OpenRouterModel::new(self.model.as_str())
                    .base_url(self.base_url.clone())
                    .title("orca");
                if let Some(key) = &self.api_key {
                    model = model.api_key(key.clone());
                }
                Arc::new(model)
            }
            Provider::OpenAi | Provider::Local => {
                let mut model =
                    OpenAiModel::new(self.model.as_str()).base_url(self.base_url.clone());
                if let Some(key) = &self.api_key {
                    model = model.api_key(key.clone());
                }
                Arc::new(model)
            }
        }
    }
}

/// Owns the Agent and the conversation; runs prompts sent by the UI.
/// `build` produces a fresh agent for the current endpoint; the
/// conversation context survives model and provider swaps.
async fn worker<F>(
    mut agent: Agent<Arc<dyn Model>>,
    system: String,
    mut endpoint: Endpoint,
    build: F,
    mut commands: mpsc::UnboundedReceiver<WorkerCmd>,
    ui: mpsc::UnboundedSender<UiMsg>,
) where
    F: Fn(&Endpoint) -> Agent<Arc<dyn Model>>,
{
    let mut context = Context::new();
    context.push_system(&system);
    while let Some(command) = commands.recv().await {
        match command {
            WorkerCmd::Run { prompt, cancel } => {
                context.push_user(&prompt);
                let result = agent.run_context(&mut context, cancel).await;
                repair_dangling_tool_calls(&mut context);
                let done = UiMsg::RunDone(result.map_err(|e| e.to_string()));
                if ui.send(done).is_err() {
                    return;
                }
            }
            WorkerCmd::Clear => {
                context = Context::new();
                context.push_system(&system);
            }
            WorkerCmd::ListModels { filter } => {
                // Detached: a slow catalog fetch must not wedge the worker
                // (runs and model switches would queue behind it).
                let ui = ui.clone();
                let base_url = endpoint.base_url.clone();
                let api_key = endpoint.api_key.clone();
                tokio::spawn(async move {
                    let result = openrouter::list_models(&base_url, api_key.as_deref())
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
            WorkerCmd::SetModel { id } => {
                endpoint.model = id;
                agent = build(&endpoint);
                if ui
                    .send(UiMsg::ModelChanged(endpoint.model.clone()))
                    .is_err()
                {
                    return;
                }
            }
            WorkerCmd::SetProvider { provider, api_key } => {
                endpoint.provider = provider;
                endpoint.base_url = provider.base_url().into();
                endpoint.api_key = api_key.or_else(|| provider.env_key());
                endpoint.model = provider.default_model().into();
                agent = build(&endpoint);
                let changed = UiMsg::ProviderChanged {
                    provider: provider.label(),
                    model: endpoint.model.clone(),
                };
                if ui.send(changed).is_err() {
                    return;
                }
            }
        }
    }
}

/// A cancelled run can leave the transcript ending in assistant tool calls
/// with no results; chat-completions endpoints reject that shape on the
/// next turn, so close them out with synthetic error results.
fn repair_dangling_tool_calls(context: &mut Context) {
    let Some(Message::Assistant { tool_calls, .. }) = context.messages().last() else {
        return;
    };
    if tool_calls.is_empty() {
        return;
    }
    let results = tool_calls
        .iter()
        .map(|call| ToolResult {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            output: serde_json::json!({"error": "cancelled before execution"}),
            is_error: true,
        })
        .collect();
    context.append_tool_results(results);
}

#[tokio::main]
async fn main() -> ExitCode {
    let cfg = match parse_args() {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };

    match cfg.theme.as_str() {
        "mono" => view::set_theme(view::mono_theme()),
        "color" => view::set_theme(view::color_theme()),
        other => {
            eprintln!("unknown theme: {other} (expected mono or color)");
            return ExitCode::FAILURE;
        }
    }

    if cfg.list_models {
        return match openrouter::list_models(&cfg.base_url, cfg.api_key.as_deref()).await {
            Ok(models) => {
                for model in models {
                    println!("{}", model.summary());
                }
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        };
    }

    run_mode(cfg).await
}

/// Headless or interactive. The endpoint (provider, base url, key, model)
/// lives in the worker and can be switched from the TUI at runtime.
async fn run_mode(cfg: Config) -> ExitCode {
    let ws = Workspace::new(&cfg.workspace);
    let system = system_prompt(&ws);
    let endpoint = Endpoint::from_config(&cfg);

    if cfg.prompt.is_some() {
        let code = headless::run(&cfg, endpoint.build_model(), &ws, &system).await;
        return ExitCode::from(code as u8);
    }

    // Interactive: worker task owns the agent; UI owns the terminal.
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

    let build = {
        let cfg = cfg.clone();
        let ui_tx = ui_tx.clone();
        move |endpoint: &Endpoint| {
            let ws = Workspace::new(&cfg.workspace);
            build_agent(endpoint.build_model(), &cfg, &ws, &ui_tx)
        }
    };
    let agent = build(&endpoint);
    tokio::spawn(worker(agent, system, endpoint, build, cmd_rx, ui_tx));

    let tui_cfg = tui::TuiConfig {
        model_name: cfg.model.clone(),
        workspace_name: cfg.workspace.display().to_string(),
    };
    match tui::run(tui_cfg, cmd_tx, ui_rx).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("terminal error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn build_agent<M: Model>(
    model: M,
    cfg: &Config,
    ws: &Workspace,
    ui: &mpsc::UnboundedSender<UiMsg>,
) -> Agent<M> {
    let events = EventStream::from_fn({
        let ui = ui.clone();
        move |event| {
            let _ = ui.send(UiMsg::Event(event));
        }
    });
    let mut agent = Agent::new(model)
        .limits(cfg.limits())
        .extension(events)
        .extension(Approval::new(ui.clone()))
        .extension(Truncation::new(16_000));
    for tool in core_tools(ws) {
        agent = agent.tool_arc(tool);
    }
    agent
}

#[cfg(test)]
mod main_tests {
    use super::*;

    /// The prompt must keep telling the model to plan and batch independent
    /// calls — dropping this silently reverts the agent to one call per turn.
    #[test]
    fn system_prompt_instructs_concurrent_batching() {
        let prompt = system_prompt(&Workspace::new(PathBuf::from(".")));
        assert!(prompt.contains("execute concurrently"));
        assert!(prompt.contains("plan the batch"));
        assert!(prompt.contains("one response"));
    }
}
