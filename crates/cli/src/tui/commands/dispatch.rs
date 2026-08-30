use super::integrations::*;

// The slash-command handlers (`/mode`, `/rewind`, `/todo`, `/skills`,
// `/mcp`, ...) behind the composer palette. Each takes the trimmed
// argument text and drives the [`App`] overlay/queue/notice machinery; a
// few hand a follow-up `WorkerCmd` to the agent for anything that needs
// the model loop (model list fetch, session load, MCP reconnect).

use tokio::sync::mpsc;

use ratatui::text::{Line, Span};

use crate::msg::{Provider, WorkerCmd};
use crate::tui::components::picker::ListPicker;
use crate::tui::components::transcript::BlockSpacing;
use crate::view::{self, theme};

use super::super::state::{App, Overlay, SESSION_ACTIONS, SETTINGS_ROWS, SUBAGENT_ROWS};
use super::super::{copy_command, expand_tool, push_error, push_notice, SESSIONS_WINDOW};

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
    app.overlay_stack.clear();
    app.picker_pending = None;
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
            app.overlay = Some(Overlay::Subagents {
                picker: ListPicker::new(SUBAGENT_ROWS),
            });
            return;
        }
        if let Some(arg) = rest.strip_prefix(' ') {
            match arg.trim().parse::<u32>() {
                Ok(depth) => {
                    let set = app.cfg.subagent_depth.set(depth);
                    let message = match crate::config::save_subagent_settings(&app.cfg.subagent_depth) {
                        Ok(_) => format!(
                            "subagent nesting depth set to {set} (saved; applies to the next spawn)"
                        ),
                        Err(err) => format!(
                            "subagent nesting depth set to {set} for this session (save failed: {err})"
                        ),
                    };
                    app.push_line(Line::from(Span::styled(message, dim)));
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
            let entries = crate::tui::mcp_picker::snapshot(&app.cfg.mcp);
            if entries.is_empty() {
                push_notice(
                    app,
                    "no MCP servers configured or loaded — /mcp add <name> <command>",
                );
                return;
            }
            app.overlay = Some(Overlay::Mcp {
                picker: ListPicker::new(entries.len()),
                filter: String::new(),
                entries,
            });
            return;
        }
        if let Some(args) = rest.strip_prefix(' ') {
            mcp_command(app, args, worker);
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("plugin") {
        if rest.is_empty() || rest.starts_with(' ') {
            plugin_command(app, rest.trim(), worker);
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("copy") {
        if rest.is_empty() || rest.starts_with(' ') {
            copy_command(app, rest.trim());
            return;
        }
    }
    if let Some(rest) = command.strip_prefix("refine") {
        if rest.is_empty() || rest.starts_with(' ') {
            super::refine_command(app, rest.trim(), worker);
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
            let request_id = app.next_picker_request;
            app.next_picker_request += 1;
            app.picker_pending = Some((request_id, rest.trim().to_lowercase()));
            if worker
                .send(WorkerCmd::ListModels {
                    request_id,
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
            if worker.send(WorkerCmd::Clear).is_err() {
                push_error(app, "worker is gone; restart orcacode");
            }
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
        "hotkeys" => {
            hotkeys_command(app, width);
        }
        "help" | "" => {
            app.overlay = Some(Overlay::Help {
                filter: String::new(),
                picker: ListPicker::new(crate::tui::command_catalog::COMMANDS.len()),
            });
        }
        other => {
            push_error(app, format!("unknown command: /{other}"));
        }
    }
}

/// Apply a session mode: the notice, the yolo warning, and the plan
/// episode end all live here so every entry point says the same thing.
pub(crate) fn apply_mode(app: &mut App, next: crate::mode::Mode) {
    let previous = app.cfg.mode.get();
    app.cfg.mode.set(next);
    // One plain-text line, glyph included — deliberately not the
    // shared notification (its dot is accent-colored): a mode change
    // is text like any other, nothing highlighted.
    app.push_line(Line::from(Span::styled(
        format!("• {} mode · {}", next.label(), next.description()),
        theme().dim,
    )));
    // Leaving plan mode ends the episode. Report the plans the agent
    // actually wrote — observed from tool results, not guessed — and say
    // nothing when it wrote none: plan mode is also a fine way to just
    // look around, and announcing a missing file would be nagging.
    if previous == crate::mode::Mode::Plan && next != crate::mode::Mode::Plan {
        for path in app.cfg.plan.end() {
            push_notice(app, format!("plan saved to {path}"));
        }
    }
}

/// `/mode [normal|plan|yolo]` — no argument opens the standard picker,
/// preselected on the current mode, exactly like /provider and /theme.
/// A name picks directly: `mode` is a command typed hourly, so the
/// text form stays.
pub(crate) fn mode_command(app: &mut App, arg: &str) {
    use crate::mode::Mode;
    if arg.is_empty() {
        let index = Mode::ALL
            .iter()
            .position(|mode| *mode == app.cfg.mode.get())
            .unwrap_or(0);
        app.overlay = Some(Overlay::Mode {
            picker: ListPicker::with_selected(Mode::ALL.len(), index),
        });
        return;
    }
    match Mode::from_label(arg) {
        Some(mode) => apply_mode(app, mode),
        None => {
            push_error(
                app,
                format!("unknown mode: {arg} — valid modes: normal, plan, yolo"),
            );
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

/// `/hotkeys` — a scrollable inventory of the keyboard bindings handled by
/// the TUI. The catalog is shared data so additions are visible and testable.
pub(crate) fn hotkeys_command(app: &mut App, width: usize) {
    let t = theme();
    let key_width = crate::tui::command_catalog::HOTKEYS
        .iter()
        .map(|hotkey| hotkey.keys.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec![Line::from(Span::styled("  hotkeys", t.strong))];
    for hotkey in crate::tui::command_catalog::HOTKEYS {
        let text = format!(
            "  {:key_width$}  {}  {}",
            hotkey.keys, hotkey.description, hotkey.category
        );
        lines.push(Line::from(Span::styled(
            view::truncate_line(&text, width),
            t.dim,
        )));
    }
    app.push_transcript_block(lines, BlockSpacing::Tight);
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
