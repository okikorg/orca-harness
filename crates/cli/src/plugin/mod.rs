mod commands;
mod scaffold;

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PluginCommand {
    Init { name: String, language: Language },
    Validate { path: PathBuf },
    Test { path: PathBuf },
    Install { path: PathBuf },
    List,
    Inspect { name: String },
    Enable { name: String },
    Disable { name: String },
    Uninstall { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Language {
    Python,
    TypeScript,
}

const USAGE: &str = "\
USAGE:
  orcacode plugin init <name> --python|--py|--typescript|--ts
  orcacode plugin validate [PATH]
  orcacode plugin test [PATH]
  orcacode plugin install [PATH]
  orcacode plugin list
  orcacode plugin inspect <name>
  orcacode plugin enable <name>
  orcacode plugin disable <name>
  orcacode plugin uninstall <name>
";

/// Return arguments after `plugin` only when it is the first positional token.
pub(crate) fn invocation_tail(args: &[String]) -> Option<&[String]> {
    let mut index = 0;
    while index < args.len() {
        let width = match args[index].as_str() {
            "--model" | "--base-url" | "--api-key" | "--firecrawl-key" | "--workspace"
            | "--max-steps" | "--subagent-depth" | "--theme" | "--resume" | "-p" | "--prompt" => 2,
            "--openrouter" | "--list-models" | "--continue" | "--no-session" | "--plan"
            | "--yolo" | "--json" | "--auto-approve" | "-h" | "--help" => 1,
            flag if flag.starts_with('-') => return None,
            "plugin" => return Some(&args[index + 1..]),
            _ => return None,
        };
        index = index.checked_add(width)?;
    }
    None
}

pub(crate) fn parse(args: &[String]) -> Result<PluginCommand, String> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err(USAGE.into());
    };
    let tail = &args[1..];
    match command {
        "init" => parse_init(tail),
        "validate" => Ok(PluginCommand::Validate {
            path: optional_path(tail, "validate")?,
        }),
        "test" => Ok(PluginCommand::Test {
            path: optional_path(tail, "test")?,
        }),
        "install" => Ok(PluginCommand::Install {
            path: optional_path(tail, "install")?,
        }),
        "list" if tail.is_empty() => Ok(PluginCommand::List),
        "inspect" => Ok(PluginCommand::Inspect {
            name: required_name(tail, "inspect")?,
        }),
        "enable" => Ok(PluginCommand::Enable {
            name: required_name(tail, "enable")?,
        }),
        "disable" => Ok(PluginCommand::Disable {
            name: required_name(tail, "disable")?,
        }),
        "uninstall" => Ok(PluginCommand::Uninstall {
            name: required_name(tail, "uninstall")?,
        }),
        _ => Err(format!(
            "unknown or malformed plugin command: {command}\n\n{USAGE}"
        )),
    }
}

fn parse_init(args: &[String]) -> Result<PluginCommand, String> {
    if args.len() != 2 {
        return Err(format!(
            "plugin init requires a name and one language flag\n\n{USAGE}"
        ));
    }
    let language = match args[1].as_str() {
        "--python" | "--py" => Language::Python,
        "--typescript" | "--ts" => Language::TypeScript,
        flag => return Err(format!("unknown plugin language: {flag}\n\n{USAGE}")),
    };
    Ok(PluginCommand::Init {
        name: args[0].clone(),
        language,
    })
}

fn optional_path(args: &[String], command: &str) -> Result<PathBuf, String> {
    match args {
        [] => Ok(PathBuf::from(".")),
        [path] => Ok(PathBuf::from(path)),
        _ => Err(format!(
            "plugin {command} accepts at most one path\n\n{USAGE}"
        )),
    }
}

fn required_name(args: &[String], command: &str) -> Result<String, String> {
    match args {
        [name] => Ok(name.clone()),
        _ => Err(format!("plugin {command} requires one name\n\n{USAGE}")),
    }
}

pub(crate) async fn run(command: PluginCommand) -> Result<(), String> {
    commands::run(command).await
}
