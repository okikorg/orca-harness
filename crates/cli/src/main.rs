//! `orcacode` — the interactive terminal host for Orca Harness.
//!
//! Interactive: `orcacode` starts a streaming REPL with tool approvals.
//! Headless:    `orcacode -p "prompt"` runs once and streams to stdout.

mod approval;
mod commands;
mod components;
mod config;
mod extensions;
mod headless;
mod mcp;
mod msg;
mod skills;
mod tui;
mod view;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use tokio::sync::mpsc;

use orca_harness_core::{Agent, Context, Limits, Message, Model, ToolResult};
use orca_harness_extensions::{
    compact, workspace_key, CompactConfig, EventStream, HarnessEvent, ReadToolResultTool,
    SessionFile, SessionHandler, Truncation, TruncationStore,
};
use orca_harness_model_openai::OpenAiModel;
use orca_harness_model_openrouter::{self as openrouter, OpenRouterModel};
use orca_harness_tools::{
    core_tools, BackgroundStats, ProcessTool, PyKernelTool, SubagentDepth, SubagentSpawn,
    SubagentTool, Workspace,
};
use orca_harness_tools_web::{Firecrawl, UrlPolicy, WebCrawlTool, WebFetchTool, WebSearchTool};

use crate::approval::Approval;
use crate::msg::{Provider, UiMsg, WorkerCmd};

const USAGE: &str = "\
orcacode — terminal host for Orca Harness

USAGE:
  orcacode [OPTIONS]                interactive session
  orcacode [OPTIONS] -p \"prompt\"    headless single run (streams to stdout)

OPTIONS:
  --model NAME       model id (env ORCA_MODEL; default qwen3.5:9b,
                     or openrouter/auto with --openrouter)
  --base-url URL     OpenAI-compatible endpoint (env ORCA_BASE_URL;
                     default: api.openai.com if OPENAI_API_KEY is set,
                     otherwise http://localhost:11434/v1)
  --api-key KEY      bearer token (env OPENAI_API_KEY, or
                     OPENROUTER_API_KEY with --openrouter)
  --firecrawl-key K  Firecrawl key (env FIRECRAWL_API_KEY); enables the
                     web_search and web_crawl tools
  --openrouter       use OpenRouter (openrouter.ai) as the endpoint
  --list-models      print the endpoint's model catalog and exit
  --workspace DIR    tool workspace root (default: current directory)
  --max-steps N      model invocations per run (default 48)
  --subagent-depth N subagent nesting levels, 1-5 (env ORCA_SUBAGENT_DEPTH;
                     default 1; /subagents adjusts it live in the TUI)
  --theme NAME       theme: default, mono, dracula,
                     solarized-dark, one-dark, monokai, nord
  --continue         resume the latest recorded session for this workspace
  --resume ID        resume a recorded session by id (a unique prefix works)
  --no-session       do not record this session to disk
  --json             headless: emit NDJSON harness events on stdout
  --auto-approve     headless: allow shell/write/edit without approval
  -p, --prompt TEXT  headless prompt
  -h, --help         show this help

In the TUI, /models [filter] opens the model catalog and picker and
/settings shows and changes the provider, model, theme, and api key.
API keys entered in the TUI, the active provider, the theme, and the
last model picked per provider are saved to
~/.config/orcacode/config.json and reused on later runs; flags and
environment variables above always win over the saved values.
Approval prompts accept y (once), a (always, this session), A (always,
saved for this workspace only; revoke in /settings), and n (deny).
Sessions are recorded under ~/.config/orcacode/sessions/ per workspace;
/sessions in the TUI lists and resumes them.
";

#[derive(Clone)]
pub struct Config {
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub firecrawl_key: Option<String>,
    pub openrouter: bool,
    pub list_models: bool,
    pub workspace: PathBuf,
    pub prompt: Option<String>,
    pub json: bool,
    pub auto_approve: bool,
    pub max_steps: u32,
    pub subagent_depth: u32,
    pub continue_latest: bool,
    pub resume_id: Option<String>,
    pub no_session: bool,
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
    let mut firecrawl_key: Option<String> = None;
    let mut openrouter = false;
    let mut list_models = false;
    let mut workspace = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut prompt = None;
    let mut json = false;
    let mut auto_approve = false;
    let mut max_steps = 48;
    let mut subagent_depth: u32 = std::env::var("ORCA_SUBAGENT_DEPTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let mut continue_latest = false;
    let mut resume_id: Option<String> = None;
    let mut no_session = false;
    let mut theme = std::env::var("ORCA_THEME").ok();

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
            "--firecrawl-key" => firecrawl_key = Some(value("--firecrawl-key")?),
            "--openrouter" => openrouter = true,
            "--list-models" => list_models = true,
            "--workspace" => workspace = PathBuf::from(value("--workspace")?),
            "--max-steps" => {
                max_steps = value("--max-steps")?
                    .parse()
                    .map_err(|_| "--max-steps expects a number".to_string())?
            }
            "--subagent-depth" => {
                subagent_depth = value("--subagent-depth")?
                    .parse()
                    .map_err(|_| "--subagent-depth expects a number".to_string())?
            }
            "--theme" => theme = Some(value("--theme")?),
            "--continue" => continue_latest = true,
            "--resume" => resume_id = Some(value("--resume")?),
            "--no-session" => no_session = true,
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

    // The provider saved by the last session applies only when nothing
    // explicit picked one (--openrouter, --base-url, or ORCA_BASE_URL).
    let stored_provider = if openrouter || base_url.is_some() {
        None
    } else {
        config::stored_provider().and_then(|label| Provider::from_label(&label))
    };
    let openrouter = openrouter || stored_provider == Some(Provider::OpenRouter);
    let provider = if openrouter {
        Provider::OpenRouter
    } else if stored_provider == Some(Provider::Local) {
        Provider::Local
    } else {
        Provider::OpenAi
    };

    // An explicit --api-key wins; otherwise the endpoint's env var, then
    // a key saved to the config file by a previous session.
    let api_key = api_key.or_else(|| provider.resolve_key());
    let firecrawl_key = firecrawl_key.or_else(|| std::env::var("FIRECRAWL_API_KEY").ok());
    let base_url = base_url.unwrap_or_else(|| {
        if openrouter {
            openrouter::OPENROUTER_BASE_URL.into()
        } else if api_key.is_some() {
            "https://api.openai.com/v1".into()
        } else {
            "http://localhost:11434/v1".into()
        }
    });
    let model = model
        .or_else(|| config::stored_model(provider.label()))
        .unwrap_or_else(|| {
            if openrouter {
                // OpenRouter's auto-router: always a valid id.
                "openrouter/auto".into()
            } else {
                "qwen3.5:9b".into()
            }
        });
    let theme = resolve_theme(theme);

    Ok(Config {
        model,
        base_url,
        api_key,
        firecrawl_key,
        openrouter,
        list_models,
        workspace,
        prompt,
        json,
        auto_approve,
        max_steps,
        subagent_depth,
        continue_latest,
        resume_id,
        no_session,
        theme,
    })
}

/// The canonicalized workspace root, used as the key for this
/// workspace's saved tool approvals. Canonicalizing keeps the key
/// stable however the directory was spelled on the command line.
fn workspace_scope(ws: &Workspace) -> String {
    ws.root()
        .canonicalize()
        .unwrap_or_else(|_| ws.root().to_path_buf())
        .display()
        .to_string()
}

/// --theme and ORCA_THEME beat the saved preference; default otherwise.
fn resolve_theme(explicit: Option<String>) -> String {
    explicit
        .or_else(config::stored_theme)
        .unwrap_or_else(|| "default".into())
}

/// The `skill` tool is deliberately absent from this list. Whether it is
/// registered depends on what is on disk and on `/skills` toggles, both
/// of which change mid-session — and this string is built once and
/// re-pushed verbatim by `WorkerCmd::Clear`, so anything conditional
/// written here goes stale. The tool's own description explains what a
/// skill is and lists the catalog, and that is rebuilt with the agent.
fn system_prompt(ws: &Workspace, web_search: bool) -> String {
    let web_tools = if web_search {
        ", web_fetch (fetch a URL as markdown), web_search, and web_crawl \
         (read a whole site section via Firecrawl)"
    } else {
        ", and web_fetch (fetch a URL as markdown)"
    };
    format!(
        "You are Orca Code, a coding agent operating in the workspace at {root} on {os}. \
         You act through tools: shell, process (persistent sessions and background \
         processes), pykernel (persistent Python — variables survive across calls; \
         print what you need to see), subagent (spawn an independent agent with its \
         own context and tools for a self-contained task; parallel calls fan out), \
         read_file, write_file, edit_file, list_dir, grep, glob, \
         read_tool_result (re-read the full output of a truncated result){web_tools}. \
         File paths are workspace-relative. Investigate with tools instead of guessing; \
         run commands to verify your work. Keep responses brief and concrete: report \
         what you did and what you found.\n\
         \n\
         Plan before every tool call. Ask what you already know, what you still need, \
         and what the smallest set of calls is that gets it. Never fire a call whose \
         result you have no plan to use, and never re-derive something a previous \
         call already told you.\n\
         \n\
         Use the harness's full concurrency. Tool calls issued in the same response \
         execute concurrently. Before acting, plan the batch: decide everything you \
         can learn or do right now that does not depend on another call's result, and \
         issue all of those calls together in one response — reading several files, \
         running independent searches, executing unrelated commands, or fanning out \
         several subagents should be one batch, not a sequence of turns. Serialize \
         only when a call's input genuinely requires another call's output. Push \
         long-running work into background processes and keep working while it runs. \
         Writes to the same file are ordered for you; unrelated writes are safe to \
         batch.\n\
         \n\
         Use pykernel as your working state. Its variables persist across calls, so \
         parse, compute, and accumulate there instead of re-running shell pipelines \
         to re-derive the same data: load results into variables once, refine them in \
         later calls, and keep intermediate findings (file lists, parsed output, \
         counters, partial conclusions) alive in the kernel rather than in your head.",
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
                    .title("orcacode");
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

/// Discover the active model's context window from the endpoint, best
/// effort, and report it to the UI. OpenRouter's catalog carries
/// `context_length`; a local ollama exposes it via the native
/// `/api/show`. Plain OpenAI endpoints publish nothing — `None`.
fn spawn_window_probe(endpoint: &Endpoint, ui: mpsc::UnboundedSender<UiMsg>) {
    let base_url = endpoint.base_url.clone();
    let api_key = endpoint.api_key.clone();
    let model = endpoint.model.clone();
    tokio::spawn(async move {
        let window = if base_url.contains("openrouter") {
            openrouter::list_models(&base_url, api_key.as_deref())
                .await
                .ok()
                .and_then(|models| models.into_iter().find(|m| m.id == model))
                .and_then(|m| m.context_length)
        } else if base_url.contains("localhost:11434") || base_url.contains("127.0.0.1:11434") {
            ollama_context_window(&base_url, &model).await
        } else {
            None
        };
        let _ = ui.send(UiMsg::ContextWindow(window));
    });
}

/// Ollama native `/api/show`: prefer an explicit `num_ctx` parameter (the
/// serving window) over the model's trained maximum (`*.context_length`).
async fn ollama_context_window(base_url: &str, model: &str) -> Option<u64> {
    let host = base_url.trim_end_matches('/').trim_end_matches("/v1");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;
    let body: serde_json::Value = client
        .post(format!("{host}/api/show"))
        .json(&serde_json::json!({"model": model}))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let num_ctx = body["parameters"].as_str().and_then(|params| {
        params.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            (parts.next() == Some("num_ctx")).then(|| parts.next()?.parse().ok())?
        })
    });
    num_ctx.or_else(|| {
        body["model_info"]
            .as_object()?
            .iter()
            .find_map(|(key, value)| key.ends_with(".context_length").then(|| value.as_u64())?)
    })
}

/// Owns the Agent and the conversation; runs prompts sent by the UI.
/// `build` produces a fresh agent for the current endpoint; the
/// conversation context survives model and provider swaps.
#[allow(clippy::too_many_arguments)]
async fn worker<F>(
    mut agent: Agent<Arc<dyn Model>>,
    system: String,
    mut endpoint: Endpoint,
    build: F,
    store: TruncationStore,
    mcp: mcp::McpServers,
    skills: skills::Skills,
    session: Option<Arc<SessionHandler>>,
    mut context: Context,
    mut commands: mpsc::UnboundedReceiver<WorkerCmd>,
    ui: mpsc::UnboundedSender<UiMsg>,
) where
    F: Fn(&Endpoint) -> Agent<Arc<dyn Model>>,
{
    let mut user_shell_call_id = 0_u64;
    spawn_window_probe(&endpoint, ui.clone());
    while let Some(command) = commands.recv().await {
        match command {
            WorkerCmd::Run { prompt, cancel } => {
                context.push_user(&prompt);
                let result = agent.run_context(&mut context, cancel).await;
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
                // Best-effort preference cache; a failed write only means
                // the next session starts on the provider default.
                let _ = config::save_model(endpoint.provider.label(), &endpoint.model);
                agent = build(&endpoint);
                spawn_window_probe(&endpoint, ui.clone());
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
                endpoint.api_key = api_key.or_else(|| provider.resolve_key());
                endpoint.model = provider.default_model().into();
                let _ = config::save_provider(provider.label());
                agent = build(&endpoint);
                spawn_window_probe(&endpoint, ui.clone());
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

/// Resolves when the process receives SIGTERM or SIGHUP (terminal window
/// closed). Children now live in their own process groups, so the CLI
/// must exit its run loop cleanly for the Drop-time group kills to fire.
#[cfg(unix)]
pub(crate) async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = hup.recv() => {}
    }
}

#[cfg(not(unix))]
pub(crate) async fn shutdown_signal() {
    std::future::pending::<()>().await
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

    match view::ThemeName::from_str(&cfg.theme) {
        Some(name) => view::set_theme(name),
        None => {
            eprintln!(
                "unknown theme: {} (expected default, mono, dracula, solarized-dark, one-dark, monokai, or nord)",
                cfg.theme
            );
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

/// Create a fresh session file, or resume one when --continue/--resume
/// asked. Errors are fatal at startup: recording (or the requested
/// resume) cannot happen, and silently running without it would lose
/// the transcript the user asked to keep.
fn open_session(cfg: &Config, ws: &Workspace) -> Result<(SessionHandler, Option<Context>), String> {
    let base = config::sessions_dir().ok_or("no home directory for session storage")?;
    let scope = workspace_scope(ws);
    let dir = base.join(workspace_key(&scope));
    if cfg.continue_latest || cfg.resume_id.is_some() {
        let sessions = SessionFile::list(&dir);
        let picked = match &cfg.resume_id {
            Some(id) => sessions
                .into_iter()
                .find(|s| s.meta.id.starts_with(id.as_str())),
            None => sessions.into_iter().next(),
        };
        let picked =
            picked.ok_or_else(|| format!("no session to resume under {}", dir.display()))?;
        let (handler, loaded) = SessionHandler::resume(&picked.path).map_err(|e| e.to_string())?;
        for warning in &loaded.warnings {
            eprintln!("warning: {warning}");
        }
        Ok((handler, Some(loaded.context)))
    } else {
        let handler =
            SessionHandler::create(&dir, &scope, &cfg.model).map_err(|e| e.to_string())?;
        Ok((handler, None))
    }
}

/// Headless or interactive. The endpoint (provider, base url, key, model)
/// lives in the worker and can be switched from the TUI at runtime.
async fn run_mode(cfg: Config) -> ExitCode {
    let ws = Workspace::new(&cfg.workspace);
    // Scanned before the prompt is built: whether the `skill` tool gets
    // advertised depends on whether any skill was found, and the scan is
    // a handful of read_dir calls.
    let skills = skills::Skills::for_session(&cfg.workspace);
    let skill_notices = skills.reload();
    let system = system_prompt(&ws, cfg.firecrawl_key.is_some());
    let endpoint = Endpoint::from_config(&cfg);
    let subagent_depth = SubagentDepth::new(cfg.subagent_depth);
    let stats = BackgroundStats::new();

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
        for line in &skill_notices {
            eprintln!("{line}");
        }
        let code = headless::run(
            &cfg,
            endpoint.build_model(),
            &ws,
            &system,
            handler,
            resumed,
            &skills,
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
    // Connect the configured MCP servers before the first build so their
    // tools are in the first agent; status lines land in the transcript
    // once the TUI starts draining the channel.
    let mcp = mcp::McpServers::new();
    for line in mcp.reload().await {
        let _ = ui_tx.send(UiMsg::Notice(line));
    }
    // The skills scan already ran (the system prompt depended on it);
    // replay whatever it had to say now that there is a transcript.
    for line in skill_notices {
        let _ = ui_tx.send(UiMsg::Notice(line));
    }
    let build = {
        let cfg = cfg.clone();
        let ui_tx = ui_tx.clone();
        let subagent_depth = subagent_depth.clone();
        let stats = stats.clone();
        let store = store.clone();
        let session = session.clone();
        let mcp = mcp.clone();
        let skills = skills.clone();
        move |endpoint: &Endpoint| {
            let ws = Workspace::new(&cfg.workspace);
            build_agent(
                endpoint.build_model(),
                &cfg,
                &ws,
                &ui_tx,
                &subagent_depth,
                &stats,
                &store,
                &session,
                &mcp,
                &skills,
            )
        }
    };
    let agent = build(&endpoint);
    // Everything above is the cold-start path — arg parse, config load,
    // skills scan, system prompt, session open, MCP connect, agent build.
    // Everything below needs a terminal, so `ORCA_BENCH` stops here: it is
    // what lets `benchmarks/startup.sh` time the whole startup without a
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
    tokio::spawn(worker(
        agent, system, endpoint, build, store, mcp, skills, session, context, cmd_rx, ui_tx,
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
fn build_agent<M: Model + Clone + 'static>(
    model: M,
    cfg: &Config,
    ws: &Workspace,
    ui: &mpsc::UnboundedSender<UiMsg>,
    subagent_depth: &SubagentDepth,
    stats: &BackgroundStats,
    store: &TruncationStore,
    session: &Option<Arc<SessionHandler>>,
    mcp: &mcp::McpServers,
    skills: &skills::Skills,
) -> Agent<M> {
    let model_for_subagents = model.clone();
    let events = EventStream::from_fn({
        let ui = ui.clone();
        move |event| {
            let _ = ui.send(UiMsg::Event(event));
        }
    });
    let mut agent = Agent::new(model)
        .limits(cfg.limits())
        .extension(events)
        .extension(Approval::new(ui.clone(), workspace_scope(ws)));
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
    for tool in core_tools(ws) {
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
        PyKernelTool::new().working_dir(root).stats(stats.clone()),
    ));
    let ui_events = ui.clone();
    let mut subagent = SubagentTool::new(model_for_subagents, ws)
        .max_depth(subagent_depth.clone())
        .stats(stats.clone());
    if extensions::enabled("retry") {
        // Inner agents get the same retry policy as the orchestrator:
        // three attempts, and data failures (nonzero exit, HTTP 5xx)
        // retry too.
        subagent = subagent.retry_with_rule(
            3,
            std::time::Duration::from_millis(250),
            extensions::data_failure,
        );
    }
    subagent = subagent.spawn_extensions(std::sync::Arc::new(move |spawn: &SubagentSpawn| {
        let ui = ui_events.clone();
        let (id, parent_id, depth) = (spawn.id, spawn.parent_id, spawn.depth);
        let call_id = spawn.call_id.clone();
        vec![std::sync::Arc::new(EventStream::from_fn(move |event| {
            let _ = ui.send(UiMsg::SubagentEvent {
                id,
                parent_id,
                depth,
                call_id: call_id.clone(),
                event,
            });
        }))
            as std::sync::Arc<dyn orca_harness_core::Extension>]
    }));
    agent = agent.tool_arc(std::sync::Arc::new(subagent));
    agent
}

#[cfg(test)]
mod main_tests {
    use super::*;

    #[test]
    fn theme_prefers_explicit_then_stored_then_default() {
        assert_eq!(resolve_theme(None), "default");
        assert_eq!(resolve_theme(Some("mono".to_string())), "mono");
        crate::config::save_theme("nord").unwrap();
        assert_eq!(resolve_theme(None), "nord");
        assert_eq!(resolve_theme(Some("mono".to_string())), "mono");
    }

    /// The prompt must keep telling the model to plan and batch independent
    /// calls — dropping this silently reverts the agent to one call per turn.
    #[test]
    fn system_prompt_instructs_concurrent_batching() {
        let prompt = system_prompt(&Workspace::new(PathBuf::from(".")), false);
        assert!(prompt.contains("execute concurrently"));
        assert!(prompt.contains("plan the batch"));
        assert!(prompt.contains("one response"));
    }

    /// The advertised tool list must match what build_agent registers:
    /// web_fetch is always on, search/crawl only with a Firecrawl key.
    #[test]
    fn system_prompt_advertises_web_tools_to_match_registration() {
        let ws = Workspace::new(PathBuf::from("."));
        let without = system_prompt(&ws, false);
        assert!(without.contains("web_fetch"));
        assert!(!without.contains("web_search"));
        let with = system_prompt(&ws, true);
        assert!(with.contains("web_search"));
        assert!(with.contains("web_crawl"));
    }

    /// `skill` is the one registered tool the prompt must *not* name.
    /// Whether it exists depends on the skill folders and on /skills
    /// toggles, both of which move mid-session, while this string is
    /// built once and re-pushed verbatim by /clear — so a mention here
    /// would eventually advertise a tool that is not registered. The
    /// tool's own description carries the explanation and the catalog.
    #[test]
    fn system_prompt_leaves_skills_to_the_tool_description() {
        let prompt = system_prompt(&Workspace::new(PathBuf::from(".")), false);
        assert!(!prompt.contains("skill"), "{prompt}");
    }

    #[test]
    fn system_prompt_advertises_pykernel_and_subagent() {
        let prompt = system_prompt(&Workspace::new(PathBuf::from(".")), false);
        assert!(prompt.contains("pykernel"));
        assert!(!prompt.contains("processes), kernel (persistent Python"));
        assert!(prompt.contains("subagent"));
    }

    /// The prompt must keep telling the model to plan ahead of each call and
    /// to lean on pykernel's persistent variables for intermediate state.
    #[test]
    fn system_prompt_instructs_planning_and_pykernel_state() {
        let prompt = system_prompt(&Workspace::new(PathBuf::from(".")), false);
        assert!(prompt.contains("Plan before every tool call"));
        assert!(prompt.contains("pykernel as your working state"));
        assert!(prompt.contains("fanning out"));
    }
}
