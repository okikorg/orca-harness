use super::super::*;
use crate::tui::components::picker::{ListPicker, PickerAction};
use crate::tui::render::matching_indices;
use crate::tui::state::SKILL_ACTIONS;
use crate::view::byte_label;

pub(crate) const PLUGIN_ACTIONS: &[PickerAction] = &[
    PickerAction {
        key: 't',
        label: "toggle",
    },
    PickerAction {
        key: 'i',
        label: "inspect",
    },
    PickerAction {
        key: 'v',
        label: "validate",
    },
    PickerAction {
        key: 'x',
        label: "test",
    },
    PickerAction {
        key: 'd',
        label: "uninstall",
    },
];

const PLUGIN_USAGE: &str = "usage: /plugin [list | init <name> --py|--ts | validate [path] | \
test [path] | install [path] | inspect <name> | enable <name> | disable <name> | uninstall <name>]";

pub(crate) fn plugin_command(app: &mut App, args: &str, worker: &mpsc::UnboundedSender<WorkerCmd>) {
    let mut parts = args.split_whitespace();
    match parts.next() {
        None | Some("list") if parts.next().is_none() => open_plugin_picker(app),
        Some("init") => {
            let (Some(name), Some(flag), None) = (parts.next(), parts.next(), parts.next()) else {
                push_error(app, PLUGIN_USAGE);
                return;
            };
            let language = match flag {
                "--python" | "--py" => crate::plugin::Language::Python,
                "--typescript" | "--ts" => crate::plugin::Language::TypeScript,
                _ => {
                    push_error(app, PLUGIN_USAGE);
                    return;
                }
            };
            let root = std::path::Path::new(&app.cfg.workspace_root);
            show_plugin_result(app, crate::plugin::init_in(root, name, language));
        }
        Some("validate") => {
            let path = plugin_path(app, command_tail(args));
            show_plugin_result(app, crate::plugin::validate(&path));
        }
        Some("test") => {
            let path = plugin_path(app, command_tail(args));
            if worker.send(WorkerCmd::TestPlugin { path }).is_err() {
                push_error(app, "worker is gone; restart orcacode");
            } else {
                push_notice(app, "testing plugin executable components…");
            }
        }
        Some("install") => {
            let path = plugin_path(app, command_tail(args));
            show_plugin_result(app, crate::plugin::install(&path));
        }
        Some("inspect") => {
            let (Some(name), None) = (parts.next(), parts.next()) else {
                push_error(app, PLUGIN_USAGE);
                return;
            };
            let result = crate::plugin::inspect(name).map(|mut lines| {
                lines.push(format!(
                    "runtime: {}",
                    plugin_runtime_note(&app.cfg.mcp, &app.cfg.skills, name)
                ));
                lines
            });
            show_plugin_result(app, result);
        }
        Some("enable") => named_plugin_action(app, parts, crate::plugin::enable),
        Some("disable") => named_plugin_action(app, parts, crate::plugin::disable),
        Some("uninstall") => {
            let (Some(name), None) = (parts.next(), parts.next()) else {
                push_error(app, PLUGIN_USAGE);
                return;
            };
            let result = crate::plugin::uninstall(name);
            let success = result.is_ok();
            show_plugin_result(app, result);
            if success {
                push_notice(
                    app,
                    "any loaded plugin skills, tools, and hooks remain available until this TUI exits",
                );
            }
        }
        _ => push_error(app, PLUGIN_USAGE),
    }
}

fn open_plugin_picker(app: &mut App) {
    let entries = crate::config::stored_plugins();
    if entries.is_empty() {
        for line in [
            "no plugins registered:",
            "  /plugin install <path>       register a local Agent Plugin",
            "  /plugin init <name> --py     scaffold Python in this workspace",
            "  /plugin init <name> --ts     scaffold TypeScript in this workspace",
        ] {
            app.push_line(Line::from(Span::styled(line, theme().dim)));
        }
        return;
    }
    app.overlay = Some(Overlay::Plugins {
        picker: ListPicker::new(entries.len()).actions(PLUGIN_ACTIONS),
        filter: String::new(),
        entries,
    });
}

fn command_tail(args: &str) -> &str {
    args.split_once(char::is_whitespace)
        .map(|(_, tail)| tail.trim())
        .unwrap_or("")
}

fn plugin_path(app: &App, path: &str) -> std::path::PathBuf {
    let path = if path.is_empty() {
        std::path::PathBuf::from(&app.cfg.workspace_root)
    } else {
        std::path::PathBuf::from(path)
    };
    if path.is_absolute() {
        path
    } else {
        std::path::Path::new(&app.cfg.workspace_root).join(path)
    }
}

fn named_plugin_action<'a>(
    app: &mut App,
    mut parts: impl Iterator<Item = &'a str>,
    action: impl FnOnce(&str) -> Result<Vec<String>, String>,
) {
    let (Some(name), None) = (parts.next(), parts.next()) else {
        push_error(app, PLUGIN_USAGE);
        return;
    };
    show_plugin_result(app, action(name));
}

fn show_plugin_result(app: &mut App, result: Result<Vec<String>, String>) {
    match result {
        Ok(lines) => {
            for line in lines {
                push_notice(app, line);
            }
        }
        Err(error) => push_error(app, error),
    }
}

fn plugin_runtime_note(
    mcp: &crate::mcp::McpServers,
    skills: &crate::skills::Skills,
    name: &str,
) -> String {
    let skill_count = skills.plugin_count(name);
    let hook_count = mcp.plugin_hook_count(name);
    let suffix = format!(" · {skill_count} skills available · {hook_count} hooks configured");
    match mcp.plugin_state(name) {
        crate::mcp::PluginRuntimeState::NotLoaded => "not loaded in this TUI".into(),
        crate::mcp::PluginRuntimeState::Starting => format!("starting{suffix}"),
        crate::mcp::PluginRuntimeState::Loaded { servers, tools } => {
            format!("loaded · {servers} servers · {tools} tools available{suffix}")
        }
        crate::mcp::PluginRuntimeState::Degraded {
            connected,
            servers,
            tools,
            warning,
        } => {
            format!("degraded · {connected}/{servers} servers · {tools} tools{suffix} · {warning}")
        }
        crate::mcp::PluginRuntimeState::Failed {
            connected,
            servers,
            tools,
            error,
        } => format!("failed · {connected}/{servers} servers · {tools} tools{suffix} · {error}"),
    }
}

pub(crate) fn plugin_picker_action(
    app: &mut App,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    registered: crate::config::RegisteredPlugin,
    action: char,
) {
    let name = registered.name.clone();
    match action {
        't' => {
            let result = if registered.enabled {
                crate::plugin::disable(&name)
            } else {
                crate::plugin::enable(&name)
            };
            let success = result.is_ok();
            show_plugin_result(app, result);
            if success {
                if let Some(Overlay::Plugins { entries, .. }) = &mut app.overlay {
                    if let Some(entry) = entries.iter_mut().find(|entry| entry.name == name) {
                        entry.enabled = !registered.enabled;
                    }
                }
            }
        }
        'i' => {
            let result = crate::plugin::inspect(&name).map(|mut lines| {
                lines.push(format!(
                    "runtime: {}",
                    plugin_runtime_note(&app.cfg.mcp, &app.cfg.skills, &name)
                ));
                lines
            });
            show_plugin_result(app, result);
        }
        'v' => show_plugin_result(app, crate::plugin::validate(&registered.root)),
        'x' => {
            if worker
                .send(WorkerCmd::TestPlugin {
                    path: registered.root,
                })
                .is_err()
            {
                push_error(app, "worker is gone; restart orcacode");
            } else {
                push_notice(app, format!("testing plugin {name} executable components…"));
            }
        }
        'd' => match crate::plugin::uninstall(&name) {
            Ok(lines) => {
                show_plugin_result(app, Ok(lines));
                push_notice(
                    app,
                    "any loaded plugin skills, tools, and hooks remain available until this TUI exits",
                );
                if let Some(Overlay::Plugins {
                    entries,
                    filter,
                    picker,
                }) = &mut app.overlay
                {
                    entries.retain(|entry| entry.name != name);
                    picker.set_len(matching_indices(entries, filter, |entry| &entry.name).len());
                    if entries.is_empty() {
                        app.overlay = None;
                    }
                }
            }
            Err(error) => push_error(app, error),
        },
        _ => {}
    }
}

pub(crate) fn skills_command(app: &mut App, args: &str, worker: &mpsc::UnboundedSender<WorkerCmd>) {
    let dim = theme().dim;
    let mut parts = args.split_whitespace();
    match parts.next() {
        None => {
            let entries = app.cfg.skills.catalog();
            if entries.is_empty() {
                for line in [
                    "no skills yet:",
                    "  /skills add <owner/repo>   install from a repository or folder",
                    "  /skills create <name>      scaffold one in .orca/skills",
                    "found automatically in .orca/skills, skills, .claude/skills,",
                    ".agents/skills, .codex/skills, .opencode/skills — and the same under ~",
                ] {
                    app.push_line(Line::from(Span::styled(line, dim)));
                }
                return;
            }
            app.overlay = Some(Overlay::Skills {
                picker: ListPicker::new(entries.len()).actions(SKILL_ACTIONS),
                filter: String::new(),
                entries,
            });
        }
        Some("add" | "install") => {
            let rest = args
                .split_once(char::is_whitespace)
                .map(|(_, rest)| rest.trim())
                .unwrap_or("");
            // `--here` is consumed here rather than in the parser: it is
            // about where this host puts things, not about the source.
            let here = rest.split_whitespace().any(|token| token == "--here");
            let source: String = rest
                .split_whitespace()
                .filter(|token| *token != "--here")
                .collect::<Vec<_>>()
                .join(" ");
            if source.is_empty() {
                for line in [
                    "usage: /skills add <source> [--skill <name>] [--list] [--here]",
                    "  source: owner/repo · owner/repo@skill · a github or skills.sh url ·",
                    "          a local folder · a pasted `npx skills add …` line",
                    "  --here installs into .orca/skills instead of your config folder",
                ] {
                    app.push_line(Line::from(Span::styled(line, dim)));
                }
                return;
            }
            let cmd = WorkerCmd::InstallSkill {
                source: source.clone(),
                here,
            };
            if worker.send(cmd).is_err() {
                push_error(app, "worker is gone; restart orcacode");
            } else {
                push_notice(app, format!("fetching {source}…"));
            }
        }
        Some("create" | "new") => {
            let Some(name) = parts.next() else {
                push_error(app, "usage: /skills create <name> [--global]");
                return;
            };
            let global = parts.any(|token| token == "--global" || token == "-g");
            match app.cfg.skills.create(name, global) {
                Ok(path) => {
                    push_notice(
                        app,
                        format!("created {} — edit it, then /skills reload", path.display()),
                    );
                    let _ = worker.send(WorkerCmd::ReloadSkills);
                }
                Err(err) => {
                    push_error(app, format!("skill not created: {err}"));
                }
            }
        }
        Some("remove" | "delete" | "rm" | "uninstall") => {
            let Some(name) = parts.next() else {
                push_error(app, "usage: /skills remove <name>");
                return;
            };
            remove_skill(app, name, worker);
        }
        Some("reload") => {
            // The rescan itself is the worker's, so the tool the agent
            // carries and the catalog on screen never disagree.
            if worker.send(WorkerCmd::ReloadSkills).is_err() {
                push_error(app, "worker is gone; restart orcacode");
            } else {
                push_notice(app, "rescanning skills…");
            }
        }
        Some("show") => {
            let Some(name) = parts.next() else {
                push_error(app, "usage: /skills show <name>");
                return;
            };
            let entries = app.cfg.skills.catalog();
            let Some(entry) = entries.iter().find(|entry| entry.name == name) else {
                let known = match entries.len() {
                    0 => "none found".to_string(),
                    _ => entries
                        .iter()
                        .map(|entry| entry.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                };
                push_error(app, format!("unknown skill: {name} — found: {known}"));
                return;
            };
            let detail = match &entry.state {
                crate::skills::SkillState::Loaded { root, bytes } => format!(
                    "{name} · {root} · {} · {}",
                    byte_label(*bytes),
                    if entry.enabled { "on" } else { "off" }
                ),
                crate::skills::SkillState::Shadowed { root, by } => {
                    format!("{name} · {root} · shadowed by the copy in {by}")
                }
                crate::skills::SkillState::Failed { root, reason } => {
                    format!("{name} · {root} · failed — {reason}")
                }
            };
            app.push_line(Line::from(Span::styled(detail, dim)));
            if !entry.description.is_empty() {
                app.push_line(Line::from(Span::styled(
                    format!("  {}", entry.description),
                    dim,
                )));
            }
        }
        Some(other) => {
            push_error(
                app,
                format!(
                    "unknown /skills argument: {other} — usage: /skills \
                     [add <source> | create <name> | remove <name> | show <name> | reload]"
                ),
            );
        }
    }
}

/// Delete an installed skill and rescan. Shared by the typed form and
/// the overlay's action strip, so both refuse the same things: only what
/// this host installed is deletable.
pub(crate) fn remove_skill(
    app: &mut App,
    name: &str,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
) -> bool {
    match app.cfg.skills.remove(name) {
        Ok(note) => {
            push_notice(app, note);
            let _ = worker.send(WorkerCmd::ReloadSkills);
            true
        }
        Err(err) => {
            push_notice(app, err);
            false
        }
    }
}

/// The typed `/mcp add|remove` forms: edit the config, then ask the
/// worker to reconnect and rebuild — the same save-then-reload shape as
/// the typed /extensions form.
pub(crate) fn mcp_command(app: &mut App, args: &str, worker: &mpsc::UnboundedSender<WorkerCmd>) {
    let usage = "usage: /mcp [add <name> <command> | remove <name>]";
    let mut parts = args.split_whitespace();
    match parts.next() {
        Some("add") => {
            let name = parts.next().unwrap_or("");
            let launch = parts.collect::<Vec<_>>().join(" ");
            if name.is_empty() || launch.is_empty() {
                push_error(app, usage);
                return;
            }
            // The name becomes part of the model-facing tool names
            // (mcp__<name>__<tool>), so keep it identifier-shaped.
            if !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                push_error(
                    app,
                    format!("invalid server name: {name} — letters, digits, - and _ only"),
                );
                return;
            }
            // Editing a server that the user turned off must not
            // quietly turn it back on, so say what actually happens
            // rather than promising a connection that will not run.
            let disabled = crate::config::stored_mcp_servers()
                .iter()
                .any(|server| server.name == name && !server.enabled);
            match crate::config::save_mcp_server(name, &launch) {
                Ok(_) => {
                    let note = if disabled {
                        format!("mcp server {name} updated — still off, space in /mcp enables it")
                    } else {
                        format!("mcp server {name} added — connecting…")
                    };
                    push_notice(app, note);
                    if worker.send(WorkerCmd::ReloadMcp).is_err() {
                        push_error(app, "worker is gone; restart orcacode");
                    }
                }
                Err(err) => {
                    push_error(
                        app,
                        format!("mcp server {name} not added (save failed: {err})"),
                    );
                }
            }
        }
        Some("remove" | "delete" | "rm") => {
            let name = parts.next().unwrap_or("");
            let servers = crate::config::stored_mcp_servers();
            if !servers.iter().any(|server| server.name == name) {
                let known = match servers.len() {
                    0 => "none configured".to_string(),
                    _ => servers
                        .iter()
                        .map(|server| server.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                };
                push_error(
                    app,
                    format!("unknown mcp server: {name} — configured: {known}"),
                );
                return;
            }
            match crate::config::remove_mcp_server(name) {
                Ok(_) => {
                    push_notice(
                        app,
                        format!("mcp server {name} removed (applies to the next run)"),
                    );
                    if worker.send(WorkerCmd::ReloadMcp).is_err() {
                        push_error(app, "worker is gone; restart orcacode");
                    }
                }
                Err(err) => {
                    push_error(
                        app,
                        format!("mcp server {name} not removed (save failed: {err})"),
                    );
                }
            }
        }
        _ => {
            push_error(app, usage);
        }
    }
}
