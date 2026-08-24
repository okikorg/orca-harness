use super::super::*;
use crate::tui::components::picker::ListPicker;
use crate::tui::format::size;
use crate::tui::state::SKILL_ACTIONS;

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
                    size(*bytes),
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
