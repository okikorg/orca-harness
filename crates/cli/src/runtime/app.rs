use std::process::ExitCode;
use std::sync::Arc;

use orca_harness_model_providers::openrouter;

use crate::msg::Provider;
use crate::parse_args;
use crate::view;

use super::interactive::run_mode;

pub(crate) async fn entrypoint() -> ExitCode {
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
        let result = if cfg.provider == Provider::OpenAiCodex {
            orca_harness_model_providers::openai_codex::list_models(Arc::new(
                crate::auth::CodexCliCredential::discover(),
            ))
            .await
        } else {
            openrouter::list_models(&cfg.base_url, cfg.api_key.as_deref()).await
        };
        return match result {
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
