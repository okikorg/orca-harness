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
mod protocol;
use orca_harness_model_providers::registry::Protocol;
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
use orca_harness_model_providers::{openrouter, ProviderModel};
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
  --base-url URL     API root for the provider's protocol (env ORCA_BASE_URL;
                     default: the provider preset's root; without a provider,
                     OpenAI if OPENAI_API_KEY is set, otherwise
                     http://localhost:11434/v1)
  --api-key KEY      API key (provider environment variable, including
                     ANTHROPIC_API_KEY for Anthropic,
                     AI_GATEWAY_API_KEY for Vercel AI Gateway,
                     CHEAPERINFERENCE_API_KEY for CheaperInference)
  --firecrawl-key K  Firecrawl key (env FIRECRAWL_API_KEY); enables the
                     web_search and web_crawl tools
  --protocol NAME    transport override for a custom --base-url (an unknown
                     name lists the choices)
  --provider ID      select a provider preset (or ORCA_PROVIDER)
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
  --orchestrate      start in orchestrate mode: the agent investigates
                     and delegates substantial work; basic edits permitted;
                     spawned workers keep their full tools
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
    pub automatic_base_url: bool,
    pub protocol: Option<Protocol>,
    pub api_key: Option<String>,
    pub firecrawl_key: Option<String>,
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
    /// Start with the top-level agent delegating to subagents instead of
    /// acting on the machine. Like the other modes, this per-session
    /// stance is not saved.
    pub orchestrate: bool,
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
        } else if self.orchestrate {
            Mode::Orchestrate
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
    openai_key: bool,
) -> Provider {
    if openrouter {
        Provider::OpenRouter
    } else if explicit_base_url {
        Provider::Local
    } else {
        // Without a saved choice, use OpenAI when a key is present, else a local server.
        stored.unwrap_or(if openai_key {
            Provider::OpenAi
        } else {
            Provider::Local
        })
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
    automatic_base_url: bool,
    protocol: Option<Protocol>,
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
            automatic_base_url: cfg.automatic_base_url,
            protocol: cfg.protocol,
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

    /// A fixed-route endpoint with no key, model or output controls.
    #[cfg(test)]
    fn fixture(provider: Provider, base_url: impl Into<String>) -> Self {
        Self {
            provider,
            base_url: base_url.into(),
            automatic_base_url: false,
            protocol: None,
            api_key: None,
            model: String::new(),
            reasoning_effort: None,
            max_output_tokens: None,
            prompt_cache: false,
            request_session_id: None,
            model_retries: Default::default(),
            model_gates: Default::default(),
            subagent_settings: Default::default(),
        }
    }

    /// The registry-backed model for this endpoint, before output controls.
    fn provider_model(&self) -> ProviderModel {
        use orca_harness_model_providers::{registry::Credential, Attribution};
        let model = ProviderModel::new(self.provider, &self.model)
            .user_agent(ORCACODE_USER_AGENT)
            .attribution(Attribution {
                referer: Some(ORCACODE_REFERER.into()),
                title: Some("Orca Code".into()),
                categories: Some("cli-agent".into()),
            })
            .prompt_cache(self.prompt_cache);
        let base_url = (!self.automatic_base_url).then_some(self.base_url.as_str());
        let session = self.request_session_id.as_deref();
        let model = with(model, self.protocol, ProviderModel::protocol);
        let model = with(model, self.api_key.as_deref(), ProviderModel::api_key);
        let model = with(model, base_url, ProviderModel::base_url);
        let model = with(model, session, ProviderModel::session_id);
        if self.provider.spec().credential != Credential::OAuth {
            return model;
        }
        model.codex_credentials(Arc::new(auth::CodexCliCredential::discover()))
    }

    async fn list_models(
        &self,
    ) -> Result<Vec<openrouter::ModelInfo>, orca_harness_core::ModelError> {
        self.provider_model().models().await
    }

    /// Explicit and saved IDs are authoritative, even when absent from discovery.
    async fn catalog_model(&self, requested: Option<&str>) -> Result<String, String> {
        if let Some(requested) = requested.filter(|model| !model.is_empty()) {
            return Ok(requested.to_string());
        }
        let help = format!(
            "specify an explicit model ID with --provider {} --model <ID>",
            self.provider.id()
        );
        let models = self
            .list_models()
            .await
            .map_err(|error| format!("{error}; {help}"))?;
        models
            .into_iter()
            .find(|model| !model.id.is_empty())
            .map(|model| model.id)
            .ok_or_else(|| {
                format!(
                    "{} returned an empty model catalog; {help}",
                    self.provider.id()
                )
            })
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
        let model = self.provider_model();
        let model = with(model, self.max_output_tokens, ProviderModel::max_tokens);
        let model = with(
            model,
            self.reasoning_effort.as_deref(),
            ProviderModel::reasoning_effort,
        );
        let model: Arc<dyn Model> = Arc::new(model);

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

/// Apply an optional setting to a builder.
fn with<M, V>(model: M, value: Option<V>, set: impl FnOnce(M, V) -> M) -> M {
    match value {
        Some(value) => set(model, value),
        None => model,
    }
}

/// Discover the active model's context window from the endpoint, best
/// effort, and report it to the UI.
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
        let window = endpoint.provider_model().context_window().await;
        if capacity.finish_update(revision, window) {
            let _ = ui.send(UiMsg::ContextWindow(window));
        }
    });
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
