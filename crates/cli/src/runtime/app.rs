use crate::view;
use crate::Endpoint;
use crate::{parse_invocation, Invocation};
use std::process::ExitCode;

use super::interactive::run_mode;

pub(crate) async fn entrypoint() -> ExitCode {
    let invocation = match parse_invocation() {
        Ok(invocation) => invocation,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let mut cfg = match invocation {
        Invocation::Update => {
            return match crate::update::run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("error: {error}");
                    ExitCode::FAILURE
                }
            };
        }
        Invocation::Plugin(command) => {
            return match crate::plugin::run(command).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("error: {error}");
                    ExitCode::FAILURE
                }
            };
        }
        Invocation::Run(cfg) => *cfg,
    };

    match view::ThemeName::from_str(&cfg.theme) {
        Some(name) => view::set_theme(name),
        None => {
            eprintln!(
                "unknown theme: {} (expected default, mono, dracula, solarized-dark, one-dark, monokai, nord, or orca)",
                cfg.theme
            );
            return ExitCode::FAILURE;
        }
    }

    if cfg.list_models {
        let result = Endpoint::from_config(&cfg).list_models().await;
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

    // Only absent models need discovery. Manual/saved IDs must
    // remain usable even when the provider cannot enumerate models.
    if cfg.model.is_empty() {
        match Endpoint::from_config(&cfg).catalog_model(None).await {
            Ok(model) => cfg.model = model,
            Err(error) => {
                eprintln!("error: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    run_mode(cfg).await
}
