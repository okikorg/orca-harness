// `orcacode` — the interactive terminal host for Orca Harness.
//
// Interactive: `orcacode` starts a streaming REPL with tool approvals.
// Headless:    `orcacode -p "prompt"` runs once and streams to stdout.

mod approval;
mod auth;
mod auto_approval;
mod changelog;
mod config;
mod extensions;
mod headless;
mod instructions;
mod mcp;
mod mode;
mod model_gates;
mod msg;
mod plan;
mod plugin;
mod presentation;
mod prompt;
mod refine;
mod run_args;
mod skills;
mod subagent_models;
mod subagent_settings;
mod tui;
mod update;
mod view;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

use orca_harness_core::{Context, Limits, Model};
use orca_harness_extensions::{RetryModel, MEMORY_GUIDANCE};
use orca_harness_model_providers::openai::OpenAiModel;
use orca_harness_model_providers::openai_codex::OpenAiCodexModel;
use orca_harness_model_providers::openrouter::{self as openrouter, OpenRouterModel};
use orca_harness_tools::Workspace;

use crate::mode::{Mode, ModeHandle};
use crate::msg::{Provider, UiMsg};
use crate::plan::PlanArea;
pub(crate) use crate::run_args::parse_run_args;

const ORCACODE_USER_AGENT: &str = concat!("orcacode/", env!("CARGO_PKG_VERSION"));
const ORCACODE_REFERER: &str = env!("CARGO_PKG_REPOSITORY");

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
  orcacode resume ID [OPTIONS]      resume a recorded session (same as --resume ID)
  orcacode update                   install the latest release
  orcacode plugin <COMMAND>         manage Agent Plugin packages

OPTIONS:
  --model NAME       model id (env ORCA_MODEL; otherwise selected from the
                     provider's live catalog)
  --base-url URL     API root; native Messages API with --anthropic,
                     otherwise OpenAI-compatible (env ORCA_BASE_URL;
                     default: api.openai.com if OPENAI_API_KEY is set,
                     otherwise http://localhost:11434/v1)
  --api-key KEY      API key (provider environment variable, including
                     ANTHROPIC_API_KEY for Anthropic,
                     AI_GATEWAY_API_KEY for Vercel AI Gateway,
                     CHEAPERINFERENCE_API_KEY for CheaperInference)
  --firecrawl-key K  Firecrawl key (env FIRECRAWL_API_KEY); enables the
                     web_search and web_crawl tools
  --openrouter       use OpenRouter (openrouter.ai) as the endpoint
  --anthropic        use the native Anthropic Messages API
  --list-models      print the endpoint's model catalog and exit
  --workspace DIR    tool workspace root (default: current directory)
  --max-steps N      model invocations per run (default 48)
  --max-output-tokens N
                     output-token cap (Anthropic default: 8192)
  --effort LEVEL     reasoning effort (for example low, medium, high)
  --prompt-cache     enable provider prompt-cache hints (default)
  --no-prompt-cache  disable provider prompt-cache hints
  --tools NAMES      headless: comma-separated tool allowlist
  --bare             headless: skip user instructions, skills, memory,
                     MCP, compute, subagents, and web tools; defaults
                     --tools to read_file,list_dir,grep,glob
  --subagent-depth N subagent nesting levels, positive integer (env ORCA_SUBAGENT_DEPTH;
                     default 1; /subagents adjusts it live in the TUI)
  --theme NAME       theme: default, mono, dracula,
                     solarized-dark, one-dark, monokai, nord, orca
  --plan             start in plan mode: read-only tools, and docs/plan/
                     the only writable directory — the agent decides
                     whether to write a plan (/mode opens a picker)
  --normal           start in normal mode (default): gated tools ask for approval
  --auto             start in auto mode: safe tools and guarded
                     file edits run; risk-bearing actions are reviewed
  --yolo             start in yolo mode: every gated tool runs without
                     approval prompts, interactive or headless. The
                     status line reads yolo for the whole session
                     (/mode opens a picker)
  --continue         resume the latest recorded session for this workspace
  --resume ID        resume a recorded session by id (a unique prefix works)
  --no-session       do not record this session to disk
  --json             headless: emit NDJSON harness events on stdout
  --auto-approve     headless: allow shell/write/edit without approval
                     (implied by --yolo)
  -p, --prompt TEXT  headless prompt
  -h, --help         show this help
  -v, -V, --version  print the version and exit

In the TUI, /provider selects local, OpenAI API, OpenRouter, Vercel AI Gateway,
CheaperInference, Anthropic, or the OpenAI Codex ChatGPT-subscription provider.
Selecting openai-codex starts Orcacode's device login; an existing
official Codex login is also imported.
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
    pub max_output_tokens: Option<u64>,
    pub reasoning_effort: Option<String>,
    pub prompt_cache: bool,
    pub tools: Option<Vec<String>>,
    pub bare: bool,
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
    /// Start with ordinary human approval prompts instead of the default
    /// automatic exact-action review. This per-session choice is not saved.
    pub normal: bool,
    /// Start with automatic exact-action reviews instead of human prompts.
    /// Like the other modes, this is a per-session stance and is not saved.
    pub auto: bool,
    /// Start with approvals off. Same reasoning as `plan`: not
    /// persisted, and the status line keeps saying yolo for as long
    /// as the session lives so it can never be forgotten.
    pub yolo: bool,
}

pub(crate) enum Invocation {
    Run(Box<Config>),
    Update,
    Plugin(plugin::PluginCommand),
}

impl Config {
    pub fn limits(&self) -> Limits {
        Limits {
            max_steps: self.max_steps,
            ..Limits::default()
        }
    }

    /// Safer explicit modes win ambiguous combinations. With no mode flag,
    /// normal human approval is the CLI default. `--auto-approve` alone
    /// retains the same mode while admitting gated headless actions.
    pub fn mode(&self) -> Mode {
        if self.plan {
            Mode::Plan
        } else if self.normal {
            Mode::Normal
        } else if self.auto {
            Mode::Auto
        } else if self.yolo {
            Mode::Yolo
        } else {
            Mode::Normal
        }
    }
}

fn parse_invocation() -> Result<Invocation, String> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.as_slice() == ["update"] {
        return Ok(Invocation::Update);
    }
    if let Some(tail) = plugin::invocation_tail(&args) {
        return plugin::parse(tail).map(Invocation::Plugin);
    }
    parse_run_args(args).map(|config| Invocation::Run(Box::new(config)))
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

/// --theme and ORCA_THEME beat the saved preference; Orca otherwise.
fn resolve_theme(explicit: Option<String>) -> String {
    explicit
        .or_else(config::stored_theme)
        .unwrap_or_else(|| "orca".into())
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
    let memory_guidance = MEMORY_GUIDANCE;
    format!(
        "You are Orca Code, a coding agent operating in the workspace at {root} on {os}. \
         You act through tools: shell, process (persistent sessions and background \
         processes), pykernel (persistent Python — variables survive across calls; \
         print what you need to see), bun_repl (persistent JavaScript and TypeScript — \
         variables and imports survive across calls; console.log what you need to see), \
         subagent (spawn an independent agent with its \
         own context and tools for a self-contained task; parallel calls fan out), \
         read_file, write_file, edit_file, apply_patch (preferred for coordinated \
         multi-file changes), multi_edit (ordered exact replacements and appends), list_dir, grep, glob, \
         todo_write (the task list for complex or explicitly requested planning), \
         read_tool_result (re-read the full output of a truncated result), \
         memory_search (search global and current-workspace memory), \
         memory_manage (save, update, or forget durable memory){web_tools}. \
         File paths are workspace-relative. Investigate with tools instead of guessing; \
         run commands to verify your work. Keep responses brief and concrete: report \
         what you did and what you found.\n\
         \n\
         {memory_guidance}\n\
         \n\
         Plan before every tool call. Ask what you already know, what you still need, \
         and what the smallest set of calls is that gets it. Never fire a call whose \
         result you have no plan to use, and never re-derive something a previous \
         call already told you. Do not use todo_write by default: reach for it only if \
         the user explicitly asks for a todo plan, or when the work spans 5+ distinct \
         steps you could genuinely lose track of. For anything smaller — routine \
         follow-ups, simple requests, single-step or few-step work — skip the list and \
         just do the work. When you do keep a list, keep it current: one step \
         in_progress, finished steps marked completed as you go.\n\
         \n\
         Delegate to subagents deliberately: they can run for many model steps. Give each \
         subagent one bounded, self-contained task, the exact result or deliverable expected, \
         and an explicit stopping condition. Avoid open-ended delegation such as \"investigate \
         this\" without defining what evidence to return and when to stop.\n\
         \n\
         Use the harness's full concurrency. Tool calls issued in the same response \
         execute concurrently. Before acting, plan the batch: decide everything you \
         can learn or do right now that does not depend on another call's result, and \
         issue all of those calls together in one response — reading several files, \
         running independent searches, executing unrelated commands, or fanning out \
         several subagents should be one batch, not a sequence of turns. Serialize \
         only when a call's input genuinely requires another call's output. Push \
         long-running work into background processes and keep working while it runs. \
         When only background work remains, block on it with the waiting action of the \
         tool that started it, or end your turn and you will be woken with the results; \
         never sleep-and-poll or repeat list calls. \
         Writes to the same file are ordered for you; unrelated writes are safe to \
         batch.\n\
         \n\
         Use pykernel for persistent Python state and bun_repl for persistent JavaScript \
         or TypeScript state. Parse, compute, and accumulate in the language that fits \
         the task instead of re-running shell pipelines to re-derive the same data: load \
         results into variables once, refine them in later calls, and keep intermediate \
         findings (file lists, parsed output, counters, partial conclusions) alive in a \
         compute session rather than in your head.",
        root = ws.root().display(),
        os = std::env::consts::OS,
    )
}

/// The worker's current endpoint: which provider, where, with what key,
/// and which model id is active. Type-erasing the adapter behind
/// `Arc<dyn Model>` is what lets one session switch providers.
#[derive(Clone)]
struct Endpoint {
    provider: Provider,
    base_url: String,
    api_key: Option<String>,
    model: String,
    reasoning_effort: Option<String>,
    max_output_tokens: Option<u64>,
    prompt_cache: bool,
    request_session_id: Option<String>,
    model_retries: Arc<AtomicU64>,
    model_gates: model_gates::ModelGates,
    subagent_settings: orca_harness_tools::SubagentDepth,
}

impl Endpoint {
    fn from_config(cfg: &Config) -> Self {
        let subagent_settings = subagent_settings::configured(
            cfg.subagent_depth,
            std::env::var("ORCA_MODEL_CONCURRENCY")
                .ok()
                .and_then(|value| value.parse().ok()),
        );
        Self {
            provider: cfg.provider,
            base_url: cfg.base_url.clone(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            reasoning_effort: cfg.reasoning_effort.clone(),
            max_output_tokens: cfg.max_output_tokens,
            prompt_cache: cfg.prompt_cache,
            request_session_id: cfg
                .prompt_cache
                .then(orca_harness_extensions::new_session_id),
            model_retries: Arc::new(AtomicU64::new(0)),
            model_gates: Default::default(),
            subagent_settings,
        }
    }

    async fn list_models(
        &self,
    ) -> Result<Vec<openrouter::ModelInfo>, orca_harness_core::ModelError> {
        match self.provider {
            Provider::Anthropic => {
                orca_harness_model_providers::anthropic::list_models(
                    &self.base_url,
                    self.api_key.as_deref(),
                )
                .await
            }
            Provider::OpenAiCodex => {
                orca_harness_model_providers::openai_codex::list_models(Arc::new(
                    auth::CodexCliCredential::discover(),
                ))
                .await
            }
            Provider::Vercel => {
                orca_harness_model_providers::vercel::list_models(self.api_key.as_deref()).await
            }
            Provider::CheaperInference => {
                orca_harness_model_providers::cheaperinference::list_models().await
            }
            Provider::OpenRouter | Provider::OpenAi | Provider::Local => {
                openrouter::list_models(&self.base_url, self.api_key.as_deref()).await
            }
        }
    }

    /// Keep a requested or saved model only while the provider still advertises
    /// it. Otherwise use the first catalog entry, whose ordering is the
    /// provider adapter's default-selection policy.
    async fn catalog_model(&self, requested: Option<&str>) -> Result<String, String> {
        let models = self
            .list_models()
            .await
            .map_err(|error| error.to_string())?;
        if let Some(requested) = requested.filter(|model| !model.is_empty()) {
            if models.iter().any(|model| model.id == requested) {
                return Ok(requested.to_string());
            }
        }
        models
            .first()
            .map(|model| model.id.clone())
            .ok_or_else(|| format!("{} returned an empty model catalog", self.provider.label()))
    }

    fn build_model(&self) -> Arc<dyn Model> {
        self.build_model_for_ui_with_label(None, None)
    }

    fn model_retry_counter(&self) -> Arc<AtomicU64> {
        self.model_retries.clone()
    }

    fn build_model_for_ui(&self, ui: Option<mpsc::UnboundedSender<UiMsg>>) -> Arc<dyn Model> {
        self.build_model_for_ui_with_label(ui, None)
    }

    fn build_model_for_ui_with_label(
        &self,
        ui: Option<mpsc::UnboundedSender<UiMsg>>,
        retry_label: Option<String>,
    ) -> Arc<dyn Model> {
        let model: Arc<dyn Model> = match self.provider {
            Provider::Anthropic => {
                let mut model = orca_harness_model_providers::AnthropicModel::new(&self.model)
                    .base_url(&self.base_url)
                    .prompt_cache(self.prompt_cache);
                if let Some(key) = &self.api_key {
                    model = model.api_key(key);
                }
                if let Some(max_tokens) = self.max_output_tokens {
                    model = model.max_tokens(max_tokens);
                }
                if let Some(effort) = &self.reasoning_effort {
                    model = model.reasoning_effort(effort);
                }
                Arc::new(model)
            }
            Provider::OpenRouter => {
                let mut model = OpenRouterModel::new(self.model.as_str())
                    .base_url(self.base_url.clone())
                    .user_agent(ORCACODE_USER_AGENT)
                    .referer(ORCACODE_REFERER)
                    .title("Orca Code")
                    .categories("cli-agent");
                if let Some(key) = &self.api_key {
                    model = model.api_key(key.clone());
                }
                if let Some(effort) = &self.reasoning_effort {
                    model = model.reasoning_effort(effort.clone());
                }
                if let Some(max_tokens) = self.max_output_tokens {
                    model = model.max_tokens(max_tokens);
                }
                if self.prompt_cache {
                    model = model.prompt_cache(true);
                    if let Some(session_id) = &self.request_session_id {
                        model = model.session_id(session_id.clone());
                    }
                }
                Arc::new(model)
            }
            Provider::Vercel | Provider::CheaperInference | Provider::OpenAi | Provider::Local => {
                let mut model = OpenAiModel::new(self.model.as_str())
                    .base_url(self.base_url.clone())
                    .user_agent(ORCACODE_USER_AGENT);
                if let Some(key) = &self.api_key {
                    model = model.api_key(key.clone());
                }
                if let Some(effort) = &self.reasoning_effort {
                    model = if self.provider == Provider::Vercel {
                        model.nested_reasoning_effort(effort.clone())
                    } else {
                        model.reasoning_effort(effort.clone())
                    };
                }
                if let Some(max_tokens) = self.max_output_tokens {
                    model = model.max_tokens(max_tokens);
                }
                if self.provider == Provider::OpenAi && self.api_key.is_some() && self.prompt_cache
                {
                    if let Some(session_id) = &self.request_session_id {
                        model = model.prompt_cache_key(session_id.clone());
                    }
                }
                Arc::new(model)
            }
            Provider::OpenAiCodex => {
                let mut model = OpenAiCodexModel::new(
                    self.model.as_str(),
                    Arc::new(auth::CodexCliCredential::discover()),
                );
                if let Some(effort) = &self.reasoning_effort {
                    model = model.reasoning_effort(effort.clone());
                }
                if self.prompt_cache {
                    if let Some(session_id) = &self.request_session_id {
                        model = model.prompt_cache_key(session_id.clone());
                    }
                }
                Arc::new(model)
            }
        };

        // A long-running turn should survive transient provider routing,
        // connection, HTTP, and timeout failures. Keep this at the shared
        // endpoint boundary so OpenRouter, OpenAI, and local compatible
        // providers all receive the same policy.
        let retries = self.model_retries.clone();
        let settings = self.subagent_settings.clone();
        let model = RetryModel::new(model, None)
            .config(move || subagent_settings::model_retry_config(&settings))
            .gate(self.model_gates.for_endpoint(self))
            .retry_delay(orca_harness_model_providers::http_error::retry_delay)
            .on_retry(move |attempt, _max_attempts, _error| {
                retries.fetch_add(1, Ordering::Relaxed);
                // One compact notification is enough. The final run error
                // retains the provider detail if every attempt fails.
                if attempt == 2 {
                    if let Some(ui) = &ui {
                        let label = retry_label.as_deref().unwrap_or("parent");
                        let _ = ui.send(UiMsg::Notice(format!(
                            "model request failed · {label} · retrying"
                        )));
                    }
                }
            });
        Arc::new(model)
    }
}

/// Discover the active model's context window from the endpoint, best
/// effort, and report it to the UI. OpenRouter's catalog carries
/// `context_length`; a local ollama exposes it via the native
/// `/api/show`. Plain OpenAI endpoints publish nothing — `None`.
fn spawn_window_probe(
    endpoint: &Endpoint,
    ui: mpsc::UnboundedSender<UiMsg>,
    capacity: orca_harness_extensions::ContextCapacity,
) {
    // Never let a newly selected model inherit the previous model's limit,
    // or let a slower probe for an older selection overwrite a newer one.
    let revision = capacity.begin_update();
    let endpoint = endpoint.clone();
    tokio::spawn(async move {
        let window = match endpoint.provider {
            // `/models` lists dated ids, so a configured alias never matches
            // the catalog; resolve the one model instead of paging all of them.
            Provider::Anthropic => orca_harness_model_providers::anthropic::retrieve_model(
                &endpoint.base_url,
                endpoint.api_key.as_deref(),
                &endpoint.model,
            )
            .await
            .ok()
            .and_then(|model| model.context_length),
            Provider::OpenAiCodex
            | Provider::OpenRouter
            | Provider::Vercel
            | Provider::CheaperInference => endpoint
                .list_models()
                .await
                .ok()
                .and_then(|models| {
                    models
                        .into_iter()
                        .find(|candidate| candidate.id == endpoint.model)
                })
                .and_then(|candidate| candidate.context_length),
            Provider::Local => ollama_context_window(&endpoint.base_url, &endpoint.model).await,
            Provider::OpenAi => None,
        };
        if capacity.finish_update(revision, window) {
            let _ = ui.send(UiMsg::ContextWindow(window));
        }
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
    // Loading the operating system's trust store is ~100ms of CPU, and the
    // first HTTPS request needs it. Start it on its own thread here so it
    // overlaps the cold-start path instead of sitting in front of it.
    orca_harness_model_providers::http::warm();
    runtime::entrypoint().await
}
