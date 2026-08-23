//! The slash-command handlers (`/mode`, `/rewind`, `/todo`, `/skills`,
//! `/mcp`, ...) behind the composer palette. Each takes the trimmed
//! argument text and drives the [`App`] overlay/queue/notice machinery; a
//! few hand a follow-up `WorkerCmd` to the agent for anything that needs
//! the model loop (model list fetch, session load, MCP reconnect).

use tokio::sync::mpsc;

use ratatui::text::{Line, Span};

use crate::components::picker::ListPicker;
use crate::components::transcript::BlockSpacing;
use crate::msg::{Provider, WorkerCmd};
use crate::view::{self, theme};

use super::format::size;
use super::render::reset_conversation_ui;
use super::state::{App, Overlay, SESSION_ACTIONS, SETTINGS_ROWS, SKILL_ACTIONS};
use super::{copy_command, expand_tool, push_error, push_notice, SESSIONS_WINDOW};

pub(crate) fn slash_command(
    app: &mut App,
    command: &str,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    width: usize,
) {
    // A slash command is user activity even though it is not a model turn.
    // Dismiss the empty-state card before writing command output so `/help`
    // (and command errors/notices) are visible on the first frame afterward.
    app.welcome_dismissed = true;
    let dim = theme().dim;
    if command == "queue" {
        let queued = app.prompt_queue.len();
        let message = match queued {
            0 => "queue empty".to_string(),
            1 => "1 prompt queued".to_string(),
            _ => format!("{queued} prompts queued"),
        };
        app.push_line(Line::from(Span::styled(message, dim)));
        return;
    }
    if let Some(rest) = command.strip_prefix("queue ") {
        if rest.trim() == "clear" {
            let cleared = app.prompt_queue.len();
            app.prompt_queue.clear();
            let message = match cleared {
                0 => "queue already empty".to_string(),
                1 => "cleared 1 queued prompt".to_string(),
                _ => format!("cleared {cleared} queued prompts"),
            };
            app.push_line(Line::from(Span::styled(message, dim)));
            return;
        }
        push_error(app, "usage: /queue [clear]");
        return;
    }
    if let Some(rest) = command.strip_prefix("expand") {
        let nth = rest.trim().parse::<usize>().unwrap_or(1).max(1);
        expand_tool(app, nth, width);
        return;
    }
    if let Some(rest) = command.strip_prefix("mode") {
        if rest.is_empty() || rest.starts_with(' ') {
            mode_command(app, rest.trim());
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("rewind") {
        if rest.is_empty() || rest.starts_with(' ') {
            rewind_command(app, rest.trim(), worker);
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("todo") {
        if rest.is_empty() || rest.starts_with(' ') {
            todo_command(app, width);
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("subagents") {
        if rest.is_empty() {
            push_notice(
                app,
                format!("subagent nesting depth: {}", app.cfg.subagent_depth.get()),
            );
            return;
        }
        if let Some(arg) = rest.strip_prefix(' ') {
            match arg.trim().parse::<u32>() {
                Ok(depth) => {
                    let set = app.cfg.subagent_depth.set(depth);
                    app.push_line(Line::from(Span::styled(
                        format!("subagent nesting depth set to {set} (applies to the next spawn)"),
                        dim,
                    )));
                }
                Err(_) => {
                    push_error(app, "usage: /subagents [1-5]");
                }
            }
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("extensions") {
        if rest.is_empty() {
            // Same interface as /theme and /provider: a picker overlay
            // where enter toggles the selected extension.
            app.overlay = Some(Overlay::Extensions {
                picker: ListPicker::new(crate::extensions::EXTENSIONS.len()),
            });
            return;
        }
        if let Some(args) = rest.strip_prefix(' ') {
            let mut parts = args.split_whitespace();
            let enabled = match parts.next() {
                Some("enable" | "add" | "on") => true,
                Some("disable" | "remove" | "delete" | "off") => false,
                _ => {
                    push_error(app, "usage: /extensions [enable|disable <name>]");
                    return;
                }
            };
            let name = parts.next().unwrap_or("");
            if crate::extensions::find(name).is_none() {
                let known = crate::extensions::EXTENSIONS
                    .iter()
                    .map(|spec| spec.name)
                    .collect::<Vec<_>>()
                    .join(", ");
                push_error(
                    app,
                    format!("unknown extension: {name} — valid extensions: {known}"),
                );
                return;
            }
            let state = if enabled { "enabled" } else { "disabled" };
            match crate::config::save_extension(name, enabled) {
                Ok(_) => {
                    push_notice(
                        app,
                        format!("extension {name} {state} (applies to the next run)"),
                    );
                    if worker.send(WorkerCmd::ReloadExtensions).is_err() {
                        push_error(app, "worker is gone; restart orcacode");
                    }
                }
                Err(err) => {
                    push_error(
                        app,
                        format!("extension {name} not {state} (save failed: {err})"),
                    );
                }
            }
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("mcp") {
        if rest.is_empty() {
            // Same interface as /extensions: a picker overlay where
            // space toggles the selected server. With nothing
            // configured the overlay would be a dead end, so the hint
            // stands in for it.
            let servers = crate::config::stored_mcp_servers();
            if servers.is_empty() {
                push_notice(app, "no MCP servers configured — /mcp add <name> <command>");
                return;
            }
            app.overlay = Some(Overlay::Mcp {
                picker: ListPicker::new(servers.len()),
                filter: String::new(),
                servers,
            });
            return;
        }
        if let Some(args) = rest.strip_prefix(' ') {
            mcp_command(app, args, worker);
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("copy") {
        if rest.is_empty() || rest.starts_with(' ') {
            copy_command(app, rest.trim());
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("skills") {
        if rest.is_empty() || rest.starts_with(' ') {
            skills_command(app, rest.trim(), worker);
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("models") {
        if rest.is_empty() || rest.starts_with(' ') {
            // Fetch the full catalog; the argument seeds the picker's
            // live filter so the user can widen it without refetching.
            app.picker_pending = Some(rest.trim().to_lowercase());
            if worker
                .send(WorkerCmd::ListModels {
                    filter: String::new(),
                })
                .is_err()
            {
                app.picker_pending = None;
                push_error(app, "worker is gone; restart orcacode");
            } else {
                push_notice(app, "fetching models…");
            }
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("sessions") {
        let arg = rest.trim();
        let Some(base) = crate::config::sessions_dir() else {
            push_error(app, "no home directory for session storage");
            return;
        };
        let dir = base.join(orca_harness_extensions::workspace_key(
            &app.cfg.workspace_root,
        ));
        let sessions = orca_harness_extensions::SessionFile::list(&dir);
        if arg.is_empty() {
            if sessions.is_empty() {
                push_notice(app, "no recorded sessions for this workspace");
                return;
            }
            // Same interface as /provider and /theme: a picker overlay,
            // preselected on the current session. The display windows to
            // the last few sessions (newest first), and ↑↓/PgUp/PgDn
            // navigate the whole list just like the /models picker.
            let index = sessions
                .iter()
                .position(|s| app.cfg.session_id.as_deref() == Some(s.meta.id.as_str()))
                .unwrap_or(0)
                // An old-but-current session stays reachable; the window
                // just opens on the newest rows.
                .min(SESSIONS_WINDOW.saturating_sub(1));
            app.overlay = Some(Overlay::Sessions {
                picker: ListPicker::with_selected(sessions.len(), index).actions(SESSION_ACTIONS),
                sessions,
            });
            return;
        }
        match sessions.iter().find(|s| s.meta.id.starts_with(arg)) {
            Some(session) => {
                if worker
                    .send(WorkerCmd::LoadSession {
                        path: session.path.clone(),
                    })
                    .is_err()
                {
                    push_error(app, "worker is gone; restart orcacode");
                } else {
                    push_notice(app, format!("loading session {}…", session.meta.id));
                }
            }
            None => {
                push_error(
                    app,
                    format!("no session matching {arg} — /sessions lists them"),
                );
            }
        }
        return;
    }
    if let Some(rest) = command.strip_prefix("theme") {
        let arg = rest.trim();
        if arg.is_empty() {
            // Same interface as /models and /provider: a picker overlay,
            // preselected on the active theme.
            let current = view::theme_name();
            let index = view::ThemeName::ALL
                .iter()
                .position(|name| *name == current)
                .unwrap_or(0);
            app.overlay = Some(Overlay::Themes {
                picker: ListPicker::with_selected(view::ThemeName::ALL.len(), index),
            });
            return;
        }
        match view::ThemeName::from_str(arg) {
            Some(name) => {
                view::set_theme(name);
                let note = match crate::config::save_theme(name.slug()) {
                    Ok(_) => format!("theme set to {}", name.label()),
                    Err(err) => format!("theme set to {} (not saved: {err})", name.label()),
                };
                push_notice(app, note);
            }
            None => {
                push_error(app, format!(
                        "unknown theme: {arg} — valid themes: default, mono, dracula, solarized-dark, one-dark, monokai, nord"
                    ));
            }
        }
        return;
    }
    match command {
        "quit" | "exit" | "q" => app.quit = true,
        "clear" => {
            let _ = worker.send(WorkerCmd::Clear);
            reset_conversation_ui(app);
        }
        "usage" => {
            app.overlay = Some(Overlay::Usage);
        }
        "compact" => {
            if worker.send(WorkerCmd::Compact).is_err() {
                push_error(app, "worker is gone; restart orcacode");
            } else {
                push_notice(app, "compacting conversation…");
            }
        }
        "fork" => {
            if worker.send(WorkerCmd::Fork).is_err() {
                push_error(app, "worker is gone; restart orcacode");
            } else {
                push_notice(app, "forking session…");
            }
        }
        "provider" => {
            app.overlay = Some(Overlay::Providers {
                picker: ListPicker::new(Provider::ALL.len()),
            });
        }
        "settings" => {
            app.overlay = Some(Overlay::Settings {
                picker: ListPicker::new(SETTINGS_ROWS),
            });
        }
        "help" | "" => {
            for entry in [
                "/help        show this help",
                "/expand [n]  full output of the n-th latest tool call (1 = latest)",
                "/clear       reset the conversation, empty the session, stop background work",
                "/compact     compact the conversation (elide tool outputs, capped summary)",
                "/usage       session token totals, cache traffic, and context occupancy",
                "/copy [code|all|tool] copy the last answer, its last code block, the transcript, or the inspected tool",
                "/sessions [id] resume a recorded session (no argument opens the picker)",
                "/queue [clear] show or clear waiting prompts",
                "/models [f]  pick a model from the endpoint's catalog",
                "/provider    switch provider (openrouter, openai, local)",
                "/settings    view and change provider, model, theme, api key",
                "/subagents [n] show or set subagent nesting depth (1-5)",
                "/extensions  toggle harness extensions (no argument opens the picker)",
                "/mcp         toggle MCP servers · add <name> <command> · remove <name>",
                "/skills      list and toggle skills (space reveals toggle/delete)",
                "/skills add <source>   install from owner/repo, a url, or a local folder",
                "/skills create <name>  scaffold a new skill · remove <name> · reload",
                "/quit        exit",
                "/theme [name] pick a color theme (no argument opens the picker)",
                "@path        add a workspace file or folder to the prompt",
                "keys: enter send or queue · esc cancel run · ctrl+o reveal latest work tree",
                "      scroll: wheel · shift+↑/↓ line · pgup/pgdn page",
                "      ctrl+c quit · up/down history",
                "      ctrl+y copy last answer",
                "copying: ctrl+y and /copy use OSC 52 (works over ssh; tmux needs set-clipboard on).",
                "         the wheel scrolls; to drag-select, hold option (macOS) or shift.",
                "         a drag selects whole terminal rows, so in split view it takes both panes;",
                "         ctrl+y copies just the focused one (tab focuses the inspector).",
                "approvals: y allow once · a always (session) · A always (saved for this workspace) · n deny",
            ] {
                app.push_line(Line::from(Span::styled(entry.to_string(), dim)));
            }
        }
        other => {
            push_error(app, format!("unknown command: /{other}"));
        }
    }
}

/// `/mode [normal|plan]` — no argument toggles, which is what a mode
/// with two states wants. The change lands on the shared handle the plan
/// gate reads per tool call, so it takes effect on the call in flight
/// with no agent rebuild and nothing to save.
pub(crate) fn mode_command(app: &mut App, arg: &str) {
    use crate::mode::Mode;
    let next = if arg.is_empty() {
        app.cfg.mode.toggle()
    } else {
        match Mode::from_label(arg) {
            Some(mode) => {
                app.cfg.mode.set(mode);
                mode
            }
            None => {
                push_error(
                    app,
                    format!("unknown mode: {arg} — valid modes: normal, plan"),
                );
                return;
            }
        }
    };
    push_notice(
        app,
        format!("{} mode · {}", next.label(), next.description()),
    );
    // Leaving plan mode ends the episode. Report the plans the agent
    // actually wrote — observed from tool results, not guessed — and say
    // nothing when it wrote none: plan mode is also a fine way to just
    // look around, and announcing a missing file would be nagging.
    if next == Mode::Normal {
        for path in app.cfg.plan.end() {
            push_notice(app, format!("plan saved to {path}"));
        }
    }
}

/// `/rewind [n]` — drop the last n user turns (default 1) from the
/// conversation and from the recorded session, so the next prompt
/// continues from before them.
pub(crate) fn rewind_command(app: &mut App, arg: &str, worker: &mpsc::UnboundedSender<WorkerCmd>) {
    let turns = if arg.is_empty() {
        1
    } else {
        match arg.parse::<usize>() {
            Ok(0) | Err(_) => {
                push_error(
                    app,
                    "usage: /rewind [n] — n is how many turns to drop (default 1)",
                );
                return;
            }
            Ok(turns) => turns,
        }
    };
    if worker.send(WorkerCmd::Rewind { turns }).is_err() {
        push_error(app, "worker is gone; restart orcacode");
    }
}

/// `/todo` — the agent's current task list, as `todo_write` last left it.
pub(crate) fn todo_command(app: &mut App, width: usize) {
    use orca_harness_tools::TodoStatus;
    let items = app.cfg.todos.items();
    if items.is_empty() {
        push_notice(app, "no task list — the agent writes one with todo_write");
        return;
    }
    let t = theme();
    let (done, total) = app.cfg.todos.progress();
    let mut lines = vec![Line::from(vec![
        Span::styled("  todo", t.strong),
        Span::styled(format!(" · {done}/{total} done"), t.dim),
    ])];
    for item in items {
        let (marker, style) = match item.status {
            TodoStatus::Completed => ("✓", t.dim),
            TodoStatus::InProgress => ("▸", t.strong),
            TodoStatus::Pending => ("□", t.dim),
        };
        lines.push(Line::from(Span::styled(
            view::truncate_line(&format!("  {marker} {}", item.content), width),
            style,
        )));
    }
    app.push_transcript_block(lines, BlockSpacing::Tight);
}

/// `/skills [add <source> | create <name> | remove <name> | show <name>
/// | reload]`.
///
/// `add` takes what the `npx skills` ecosystem takes — `owner/repo`,
/// `owner/repo@skill`, a GitHub or skills.sh URL, a local folder, even a
/// pasted `npx skills add …` line — and installs beside `config.json`
/// unless `--here` puts it in the project. `create` scaffolds a new one
/// in the project. Cloning happens in the worker, so the interface stays
/// responsive.
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
