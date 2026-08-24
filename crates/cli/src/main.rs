// `orcacode` — the interactive terminal host for Orca Harness.
//
// Interactive: `orcacode` starts a streaming REPL with tool approvals.
// Headless:    `orcacode -p "prompt"` runs once and streams to stdout.

mod approval;
mod auth;
mod config;
mod extensions;
mod headless;
mod instructions;
mod mcp;
mod mode;
mod msg;
mod plan;
mod presentation;
mod prompt;
mod skills;
mod tui;
mod view;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use tokio::sync::mpsc;

use orca_harness_core::{Context, Limits, Model};
use orca_harness_extensions::RetryModel;
use orca_harness_model_providers::openai::OpenAiModel;
use orca_harness_model_providers::openai_codex::OpenAiCodexModel;
use orca_harness_model_providers::openrouter::{self as openrouter, OpenRouterModel};
use orca_harness_tools::Workspace;

use crate::mode::{Mode, ModeHandle};
use crate::msg::{Provider, UiMsg};
use crate::plan::PlanArea;

/// What the worker needs to open a planning episode: the live mode and
/// the area a plan may be written to. Bundled because they only ever
/// travel together.
#[derive(Clone)]
struct Planning {
    mode: ModeHandle,
    area: PlanArea,
}

impl Planning {
    /// Tell the model the rules of plan mode, once per episode: what it
    /// may not do, the one directory it may write, the naming
    /// convention, and today's date (which it has no other way to know).
    ///
    /// It does **not** name a file. Whether the conversation warrants a
    /// plan at all, and what to call it, are the agent's decisions — the
    /// host cannot tell a feature request from a greeting at the moment
    /// a turn begins, and guessing produced `docs/plan/…-hi.md`.
    fn open_episode(&self, context: &mut Context) -> bool {
        if !self.mode.is_plan() || !self.area.open() {
            return false;
        }
        context.push_system(plan::briefing(&plan::today()));
        true
    }
}

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
  --plan             start in plan mode: read-only tools, and docs/plan/
                     the only writable directory — the agent decides
                     whether to write a plan (/mode toggles it)
  --continue         resume the latest recorded session for this workspace
  --resume ID        resume a recorded session by id (a unique prefix works)
  --no-session       do not record this session to disk
  --json             headless: emit NDJSON harness events on stdout
  --auto-approve     headless: allow shell/write/edit without approval
  -p, --prompt TEXT  headless prompt
  -h, --help         show this help

In the TUI, /provider selects local, OpenAI API, OpenRouter, or the
OpenAI Codex ChatGPT-subscription provider. Selecting openai-codex starts
Orcacode's device login; an existing official Codex login is also imported.
No API key is required. /models [filter] opens the model catalog,
and /settings changes the provider, model, theme, and API key.
API keys entered in the TUI, the active provider, the theme, and the
last model picked per provider are saved to
~/.config/orcacode/config.json and reused on later runs; flags and
environment variables above always win over the saved values.
Approval prompts accept y (once), a (always, this session), A (always,
saved for this workspace only; revoke in /settings), and n (deny).
Sessions are recorded under ~/.config/orcacode/sessions/ per workspace;
/sessions in the TUI lists and resumes them, /rewind drops the last
turns, and /fork branches the conversation into a new session.
Standing instructions are read at startup from AGENTS.md beside
config.json, in the workspace, and in the workspace's .orca/ folder.
";

#[derive(Clone)]
pub struct Config {
    pub provider: Provider,
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
    /// Start read-only. Deliberately not persisted to the config file:
    /// plan mode is a stance taken for a piece of work, not a setting,
    /// and a session that silently came back read-only would be a
    /// puzzle rather than a safeguard.
    pub plan: bool,
}

impl Config {
    pub fn limits(&self) -> Limits {
        Limits {
            max_steps: self.max_steps,
            ..Limits::default()
        }
    }

    /// The mode a session starts in.
    pub fn mode(&self) -> Mode {
        if self.plan {
            Mode::Plan
        } else {
            Mode::Normal
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
    let mut plan = false;
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
            "--plan" => plan = true,
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
    let provider = select_provider(openrouter, base_url.is_some(), stored_provider);

    // An explicit --api-key wins; otherwise the endpoint's env var, then
    // a key saved to the config file by a previous session.
    let api_key = api_key.or_else(|| provider.resolve_key());
    let firecrawl_key = firecrawl_key.or_else(|| std::env::var("FIRECRAWL_API_KEY").ok());
    let base_url = base_url.unwrap_or_else(|| match provider {
        Provider::OpenRouter => openrouter::OPENROUTER_BASE_URL.into(),
        Provider::OpenAiCodex => orca_harness_model_providers::openai_codex::CODEX_BASE_URL.into(),
        Provider::OpenAi if api_key.is_some() => "https://api.openai.com/v1".into(),
        Provider::OpenAi | Provider::Local => "http://localhost:11434/v1".into(),
    });
    let model = model
        .or_else(|| config::stored_model(provider.label()))
        .unwrap_or_else(|| provider.default_model().into());
    let theme = resolve_theme(theme);

    Ok(Config {
        provider,
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
        plan,
    })
}

fn select_provider(
    openrouter: bool,
    explicit_base_url: bool,
    stored: Option<Provider>,
) -> Provider {
    if openrouter {
        Provider::OpenRouter
    } else if explicit_base_url {
        Provider::Local
    } else {
        stored.unwrap_or(Provider::OpenAi)
    }
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
         todo_write (the task list for work with several steps), \
         read_tool_result (re-read the full output of a truncated result){web_tools}. \
         File paths are workspace-relative. Investigate with tools instead of guessing; \
         run commands to verify your work. Keep responses brief and concrete: report \
         what you did and what you found.\n\
         \n\
         Plan before every tool call. Ask what you already know, what you still need, \
         and what the smallest set of calls is that gets it. Never fire a call whose \
         result you have no plan to use, and never re-derive something a previous \
         call already told you. For work with several distinct steps, put the plan in \
         todo_write and keep it current — one step in_progress, finished steps marked \
         completed as you go — so the list always says where the work actually is.\n\
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
    const MODEL_MAX_ATTEMPTS: u32 = 10;

    fn from_config(cfg: &Config) -> Self {
        Self {
            provider: cfg.provider,
            base_url: cfg.base_url.clone(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
        }
    }

    fn build_model(&self) -> Arc<dyn Model> {
        self.build_model_for_ui(None)
    }

    fn build_model_for_ui(&self, ui: Option<mpsc::UnboundedSender<UiMsg>>) -> Arc<dyn Model> {
        let model: Arc<dyn Model> = match self.provider {
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
            Provider::OpenAiCodex => Arc::new(OpenAiCodexModel::new(
                self.model.as_str(),
                Arc::new(auth::CodexCliCredential::discover()),
            )),
        };

        // A long-running turn should survive transient provider routing,
        // connection, HTTP, and timeout failures. Keep this at the shared
        // endpoint boundary so OpenRouter, OpenAI, and local compatible
        // providers all receive the same policy.
        let mut model = RetryModel::new(model, Self::MODEL_MAX_ATTEMPTS);
        if let Some(ui) = ui {
            model = model.on_retry(move |attempt, max_attempts, _error| {
                // One compact notification is enough. The final run error
                // retains the provider detail if every attempt fails.
                if attempt == 2 {
                    let _ = ui.send(UiMsg::Notice(format!(
                        "model request failed · retrying up to {max_attempts} attempts"
                    )));
                }
            });
        }
        Arc::new(model)
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

mod runtime;

#[cfg(test)]
#[path = "main_tests.rs"]
mod split_main_tests;

pub(crate) use runtime::shutdown_signal;

#[tokio::main]
async fn main() -> ExitCode {
    runtime::entrypoint().await
}
