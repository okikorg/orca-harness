//! Parsing for normal interactive and headless runs.

use std::path::PathBuf;

use orca_harness_model_providers::openrouter;

use crate::msg::Provider;
use crate::{config, resolve_theme, select_provider, Config, USAGE};

pub(crate) fn parse_run_args(args: Vec<String>) -> Result<Config, String> {
    let mut model = std::env::var("ORCA_MODEL").ok();
    let mut base_url = std::env::var("ORCA_BASE_URL").ok();
    let mut api_key: Option<String> = None;
    let mut firecrawl_key: Option<String> = None;
    let mut openrouter = false;
    let mut anthropic = false;
    let mut explicit_provider: Option<String> = None;
    let mut list_models = false;
    let mut workspace = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut prompt = None;
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
            "--provider" => explicit_provider = Some(value("--provider")?),
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
            "-p" | "--prompt" => prompt = Some(value("-p")?),
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

    let explicit_provider = requested_provider(
        explicit_provider.as_deref(),
        std::env::var("ORCA_PROVIDER").ok().as_deref(),
        openrouter,
        anthropic,
    )?;
    let stored_provider =
        if explicit_provider.is_some() || openrouter || anthropic || base_url.is_some() {
            None
        } else {
            config::stored_provider().and_then(|label| Provider::from_label(&label))
        };
    let openrouter = openrouter || stored_provider == Some(Provider::OpenRouter);
    let provider = if let Some(provider) = explicit_provider {
        provider
    } else if anthropic {
        Provider::Anthropic
    } else {
        select_provider(openrouter, base_url.is_some(), stored_provider)
    };

    if max_output_tokens.is_some() && provider == Provider::OpenAiCodex {
        return Err("--max-output-tokens is not supported by openai-codex".into());
    }

    let api_key = api_key.or_else(|| provider.resolve_key());
    let firecrawl_key = firecrawl_key.or_else(|| std::env::var("FIRECRAWL_API_KEY").ok());
    let automatic_base_url = base_url.is_none();
    let base_url = if let Provider::Preset(preset) = provider {
        // Display default only; automatic routing is tracked independently.
        base_url.unwrap_or_else(|| preset.base_url().into())
    } else {
        base_url.unwrap_or_else(|| match provider {
            Provider::OpenRouter => openrouter::OPENROUTER_BASE_URL.into(),
            Provider::Vercel => Provider::Vercel.base_url().into(),
            Provider::CheaperInference => Provider::CheaperInference.base_url().into(),
            Provider::OpenAiCodex => {
                orca_harness_model_providers::openai_codex::CODEX_BASE_URL.into()
            }
            Provider::Anthropic => Provider::Anthropic.base_url().into(),
            Provider::OpenAi if api_key.is_some() => "https://api.openai.com/v1".into(),
            Provider::OpenAi | Provider::Local => "http://localhost:11434/v1".into(),
            Provider::Preset(_) => unreachable!(),
        })
    };
    let model = model
        .or_else(|| config::stored_model(provider.label()))
        .unwrap_or_else(|| provider.default_model().into());
    let theme = resolve_theme(theme);

    if prompt.is_none() && (bare || tools.is_some()) {
        return Err("--bare and --tools require headless -p/--prompt".into());
    }

    Ok(Config {
        provider,
        model,
        base_url,
        automatic_base_url,
        api_key,
        firecrawl_key,
        openrouter: provider == Provider::OpenRouter,
        list_models,
        workspace,
        prompt,
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

/// Command-line selectors beat the environment; conflicting CLI selectors are errors.
fn requested_provider(
    cli: Option<&str>,
    environment: Option<&str>,
    openrouter: bool,
    anthropic: bool,
) -> Result<Option<Provider>, String> {
    if openrouter && anthropic {
        return Err("--anthropic and --openrouter cannot be combined".into());
    }
    if cli.is_some() && (openrouter || anthropic) {
        return Err("--provider cannot be combined with --openrouter or --anthropic".into());
    }
    let id = cli.or_else(|| {
        if openrouter || anthropic {
            None
        } else {
            environment
        }
    });
    id.map(|id| {
        Provider::from_label(id).ok_or_else(|| {
            format!("unknown provider {id:?}; use --provider with a registered provider ID")
        })
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_selector_precedence_and_conflicts() {
        let deepseek = Provider::from_label("deepseek").unwrap();
        assert_eq!(
            requested_provider(Some("deepseek"), Some("invalid"), false, false).unwrap(),
            Some(deepseek)
        );
        assert_eq!(
            requested_provider(None, Some("deepseek"), false, false).unwrap(),
            Some(deepseek)
        );
        for (router, anthropic) in [(true, false), (false, true)] {
            assert_eq!(
                requested_provider(None, Some("invalid"), router, anthropic).unwrap(),
                None
            );
            assert!(requested_provider(Some("deepseek"), None, router, anthropic).is_err());
        }
        assert!(requested_provider(None, None, true, true).is_err());
        assert!(requested_provider(None, Some("invalid"), false, false).is_err());
        assert!(requested_provider(Some("invalid"), None, false, false).is_err());
    }

    #[test]
    fn provider_args_preserve_explicit_overrides() {
        let cfg = parse_run_args(
            [
                "--provider",
                "deepseek",
                "--base-url",
                "http://localhost:8080/custom",
                "--model",
                "custom-model",
                "--api-key",
                "explicit-key",
                "--max-output-tokens",
                "123",
                "--effort",
                "low",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        )
        .unwrap();
        assert_eq!(cfg.provider, Provider::from_label("deepseek").unwrap());
        assert_eq!(cfg.base_url, "http://localhost:8080/custom");
        assert_eq!(cfg.model, "custom-model");
        assert_eq!(cfg.api_key.as_deref(), Some("explicit-key"));
        assert_eq!(cfg.max_output_tokens, Some(123));
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("low"));
        for args in [
            vec!["--provider"],
            vec!["--provider", "unknown"],
            vec!["--provider", "openai-codex", "--max-output-tokens", "123"],
        ] {
            assert!(parse_run_args(args.into_iter().map(str::to_owned).collect()).is_err());
        }
    }

    #[test]
    fn explicit_preset_default_keeps_explicit_origin() {
        let provider = Provider::from_label("minimax").unwrap();
        let cfg = parse_run_args(vec!["--provider".into(), "minimax".into(),
            "--base-url".into(), provider.base_url().into()]).unwrap();
        assert_eq!(cfg.base_url, provider.base_url());
        assert!(!cfg.automatic_base_url);
    }

    #[test]
    fn default_preset_urls_keep_automatic_origin() {
        // Do not mutate process-wide environment in parallel tests.
        if std::env::var_os("ORCA_BASE_URL").is_some() {
            return;
        }
        for id in [
            "google-vertex",
            "cloudflare-ai-gateway",
            "cloudflare-workers-ai",
            "databricks-unity-gateway",
        ] {
            let cfg = parse_run_args(vec!["--provider".into(), id.into()]).unwrap();
            assert_eq!(cfg.base_url, cfg.provider.base_url());
            assert!(cfg.automatic_base_url);
        }
    }
}
