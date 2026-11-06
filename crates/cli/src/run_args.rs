//! Parsing for normal interactive and headless runs.

use std::path::PathBuf;

use orca_harness_model_providers::openrouter;

use crate::headless_input::{self, ApprovalChannel, PromptSource};
use crate::msg::Provider;
use crate::{config, resolve_theme, select_provider, Config, USAGE};

pub(crate) fn parse_run_args(args: Vec<String>) -> Result<Config, String> {
    let mut model = std::env::var("ORCA_MODEL").ok();
    let mut base_url = std::env::var("ORCA_BASE_URL").ok();
    let mut api_key: Option<String> = None;
    let mut firecrawl_key: Option<String> = None;
    let mut openrouter = false;
    let mut anthropic = false;
    let mut list_models = false;
    let mut workspace = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut prompt = None;
    let mut prompt_source: Option<PromptSource> = None;
    let mut mcp_config = Vec::new();
    let mut approvals = ApprovalChannel::Deny;
    let mut read_only = false;
    let mut images = Vec::new();
    let mut json = false;
    let mut auto_approve = false;
    let mut max_steps = 48;
    let mut max_output_tokens = None;
    let mut reasoning_effort = None;
    let mut prompt_cache = true;
    let mut tools = None;
    let mut bare = false;
    let mut subagent_depth: u32 = std::env::var("ORCA_SUBAGENT_DEPTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let mut continue_latest = false;
    let mut resume_id: Option<String> = None;
    let mut no_session = false;
    let mut plan = false;
    let mut orchestrate = false;
    let mut normal = false;
    let mut auto = false;
    let mut yolo = false;
    let mut theme = std::env::var("ORCA_THEME").ok();

    let mut args = args.into_iter().peekable();
    if args.peek().is_some_and(|arg| arg == "resume") {
        args.next();
        resume_id = Some(
            args.next()
                .filter(|id| !id.is_empty() && !id.starts_with('-'))
                .ok_or("usage: orcacode resume ID [OPTIONS]")?,
        );
    }
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
            "--anthropic" => anthropic = true,
            "--list-models" => list_models = true,
            "--workspace" => workspace = PathBuf::from(value("--workspace")?),
            "--max-steps" => {
                max_steps = value("--max-steps")?
                    .parse()
                    .map_err(|_| "--max-steps expects a number".to_string())?
            }
            "--max-output-tokens" => {
                max_output_tokens = Some(
                    value("--max-output-tokens")?
                        .parse()
                        .map_err(|_| "--max-output-tokens expects a number".to_string())?,
                )
            }
            "--effort" => reasoning_effort = Some(value("--effort")?),
            "--prompt-cache" => prompt_cache = true,
            "--no-prompt-cache" => prompt_cache = false,
            "--tools" => {
                let names = value("--tools")?
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                if names.is_empty() {
                    return Err("--tools expects at least one tool name".into());
                }
                tools = Some(names);
            }
            "--bare" => bare = true,
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
            "--orchestrate" => orchestrate = true,
            "--normal" => normal = true,
            "--auto" => auto = true,
            "--yolo" => yolo = true,
            "--json" => json = true,
            "--auto-approve" => auto_approve = true,
            "-p" | "--prompt" => {
                let text = value("-p")?;
                if prompt.is_some() || prompt_source.is_some() {
                    return Err("give the prompt once: -p TEXT, -p - or --prompt-file".into());
                }
                if text == "-" {
                    prompt_source = Some(PromptSource::Stdin);
                } else {
                    prompt = Some(text);
                }
            }
            "--prompt-file" => {
                let path = value("--prompt-file")?;
                if prompt.is_some() || prompt_source.is_some() {
                    return Err("give the prompt once: -p TEXT, -p - or --prompt-file".into());
                }
                prompt_source = Some(PromptSource::File(PathBuf::from(path)));
            }
            "--mcp-config" => {
                mcp_config.extend(headless_input::parse_mcp_config(&value("--mcp-config")?)?)
            }
            "--approvals" => approvals = ApprovalChannel::parse(&value("--approvals")?)?,
            "--read-only" => read_only = true,
            "--image" => images.push(PathBuf::from(value("--image")?)),
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-v" | "-V" | "--version" => {
                println!("orcacode {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag: {other}\n\n{USAGE}")),
        }
    }

    if openrouter && anthropic {
        return Err("--anthropic and --openrouter cannot be combined".into());
    }
    let stored_provider = if openrouter || anthropic || base_url.is_some() {
        None
    } else {
        config::stored_provider().and_then(|label| Provider::from_label(&label))
    };
    let openrouter = openrouter || stored_provider == Some(Provider::OpenRouter);
    let provider = if anthropic {
        Provider::Anthropic
    } else {
        select_provider(openrouter, base_url.is_some(), stored_provider)
    };

    if max_output_tokens.is_some() && provider == Provider::OpenAiCodex {
        return Err("--max-output-tokens is not supported by openai-codex".into());
    }

    let api_key = api_key.or_else(|| provider.resolve_key());
    let firecrawl_key = firecrawl_key.or_else(|| std::env::var("FIRECRAWL_API_KEY").ok());
    let base_url = base_url.unwrap_or_else(|| match provider {
        Provider::OpenRouter => openrouter::OPENROUTER_BASE_URL.into(),
        Provider::Vercel => Provider::Vercel.base_url().into(),
        Provider::CheaperInference => Provider::CheaperInference.base_url().into(),
        Provider::OpenAiCodex => orca_harness_model_providers::openai_codex::CODEX_BASE_URL.into(),
        Provider::Anthropic => Provider::Anthropic.base_url().into(),
        Provider::OpenAi if api_key.is_some() => "https://api.openai.com/v1".into(),
        Provider::OpenAi | Provider::Local => "http://localhost:11434/v1".into(),
    });
    let model = model
        .or_else(|| config::stored_model(provider.label()))
        .unwrap_or_else(|| provider.default_model().into());
    let theme = resolve_theme(theme);

    let headless = prompt.is_some() || prompt_source.is_some();
    if !headless && (bare || tools.is_some()) {
        return Err("--bare and --tools require headless -p/--prompt".into());
    }
    if !headless
        && (!mcp_config.is_empty()
            || approvals != ApprovalChannel::Deny
            || read_only
            || !images.is_empty())
    {
        return Err(
            "--mcp-config, --approvals, --read-only and --image require headless -p/--prompt"
                .into(),
        );
    }
    if bare && !mcp_config.is_empty() {
        return Err("--bare skips MCP; drop --bare to use --mcp-config".into());
    }
    let mut seen = std::collections::HashSet::new();
    if let Some(name) = mcp_config.iter().find(|s| !seen.insert(s.name.clone())) {
        return Err(format!(
            "--mcp-config: server {} is defined twice",
            name.name
        ));
    }
    if approvals == ApprovalChannel::Stdio {
        if !json {
            return Err("--approvals stdio requires --json".into());
        }
        if prompt_source == Some(PromptSource::Stdin) {
            return Err(
                "--approvals stdio reads decisions from stdin; pass the prompt with --prompt-file instead of -p -"
                    .into(),
            );
        }
    }

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
        prompt_source,
        mcp_config,
        approvals,
        read_only,
        images,
        json,
        auto_approve,
        max_steps,
        max_output_tokens,
        reasoning_effort,
        prompt_cache,
        tools,
        bare,
        subagent_depth,
        continue_latest,
        resume_id,
        no_session,
        theme,
        plan,
        orchestrate,
        normal,
        auto,
        yolo,
    })
}
