//! Key and mouse dispatch for the modal overlays, approval prompts, and
//! the slash palette. The big one is [`handle_overlay_key`]: every picker
//! row, toggle, and drill-down the overlays support funnels through the
//! `After` state machine so side effects (worker sends, config writes,
//! overlay replacement) happen only after the current borrow ends.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

use crate::commands::{filter_commands, CommandSpec};
use crate::components::picker::{ListPicker, PickerEvent};
use crate::components::transcript::{
    set_transcript_spacing, transcript_spacing, TranscriptSpacing,
};
use crate::msg::{ApprovalResponse, Provider, WorkerCmd};
use crate::view;

use super::format::byte_index;
use super::render::matching_indices;
use super::state::{App, LocationEntry, Overlay, ViewMode};
use super::{clear_tool_connectors, push_error, push_notice, remove_skill, PICKER_ROWS};

pub(crate) fn handle_overlay_key(
    app: &mut App,
    key: KeyEvent,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c')) {
        app.overlay = None;
        return;
    }
    // In-place edits happen under the borrow; anything that replaces the
    // overlay or talks to the worker is deferred until the borrow ends.
    enum After {
        Nothing,
        Close,
        Replace(Overlay),
        /// Send to the worker with the overlay left open (toggle rows).
        Send(WorkerCmd),
        CloseAndSend(WorkerCmd),
        /// Close, remember the picked model's context window, switch model.
        CloseAndSetModel {
            id: String,
            window: Option<u64>,
        },
        /// Close, persist, and activate the selected transcript layout.
        CloseAndSetView(ViewMode),
        /// Close, persist, and activate transcript section spacing.
        CloseAndSetTranscriptSpacing(TranscriptSpacing),
        /// Close and drop a dim status line into the history.
        CloseWithNote(String),
        /// Keep the overlay open and drop a dim status line (row
        /// actions that mutate the list in place).
        Note(String),
        /// Close, send to the worker, and drop a dim status line.
        SendWithNote(WorkerCmd, String),
        /// Close and start the /models fetch-then-pick flow.
        FetchModels,
        /// Delete an installed skill, then rescan. Deferred out of the
        /// overlay match because it needs `app` (the shared handle, the
        /// config, the transcript), which the match holds borrowed.
        RemoveSkill(String),
        /// Close and replace the active `@query` with a workspace path.
        InsertLocation {
            token_start: usize,
            entry: LocationEntry,
        },
    }
    // Read before the overlay borrow: the settings rows need these.
    let current_provider = app.cfg.provider;
    let current_view = app.view_mode;
    let current_spacing = transcript_spacing();
    let workspace_root = app.cfg.workspace_root.clone();
    let current_session = app.cfg.session_id.clone();
    let Some(overlay) = app.overlay.as_mut() else {
        return;
    };
    let after = match overlay {
        Overlay::Models(picker) => {
            let after = match key.code {
                KeyCode::Up => {
                    picker.index = picker.index.saturating_sub(1);
                    After::Nothing
                }
                KeyCode::Down => {
                    picker.index += 1;
                    After::Nothing
                }
                KeyCode::PageUp => {
                    picker.index = picker.index.saturating_sub(PICKER_ROWS);
                    After::Nothing
                }
                KeyCode::PageDown => {
                    picker.index += PICKER_ROWS;
                    After::Nothing
                }
                KeyCode::Enter => match picker.selected_info() {
                    Some((id, window)) => After::CloseAndSetModel { id, window },
                    None => After::Close,
                },
                KeyCode::Char(c) => {
                    picker.filter.push(c);
                    picker.index = 0;
                    After::Nothing
                }
                KeyCode::Backspace => {
                    picker.filter.pop();
                    picker.index = 0;
                    After::Nothing
                }
                _ => After::Nothing,
            };
            let len = picker.filtered().len();
            picker.index = picker.index.min(len.saturating_sub(1));
            after
        }
        Overlay::Locations(location) => match key.code {
            KeyCode::Tab => match location.selected() {
                Some(entry) => After::InsertLocation {
                    token_start: location.token_start,
                    entry,
                },
                None => After::Nothing,
            },
            _ => match location.picker.on_key(key.code) {
                PickerEvent::Activated(_) => match location.selected() {
                    Some(entry) => After::InsertLocation {
                        token_start: location.token_start,
                        entry,
                    },
                    None => After::Nothing,
                },
                PickerEvent::Moved | PickerEvent::Action { .. } => After::Nothing,
                PickerEvent::Ignored => match key.code {
                    KeyCode::Char(c) => {
                        location.query.push(c);
                        let at = byte_index(&app.composer, app.cursor);
                        app.composer.insert(at, c);
                        app.cursor += 1;
                        location.sync_len();
                        After::Nothing
                    }
                    KeyCode::Backspace if !location.query.is_empty() => {
                        location.query.pop();
                        let at = byte_index(&app.composer, app.cursor - 1);
                        app.composer.remove(at);
                        app.cursor -= 1;
                        location.sync_len();
                        After::Nothing
                    }
                    KeyCode::Backspace | KeyCode::Delete => {
                        let at = byte_index(&app.composer, location.token_start);
                        app.composer.remove(at);
                        app.cursor = location.token_start;
                        After::Close
                    }
                    _ => After::Nothing,
                },
            },
        },
        Overlay::Providers { picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => {
                let provider = Provider::ALL[index];
                if provider.key_env().is_some() && provider.resolve_key().is_none() {
                    // No key in the shell or config file: ask before switching.
                    After::Replace(Overlay::ApiKey {
                        provider,
                        input: String::new(),
                    })
                } else {
                    After::CloseAndSend(WorkerCmd::SetProvider {
                        provider,
                        api_key: None,
                    })
                }
            }
            _ => After::Nothing,
        },
        Overlay::Themes { picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => {
                let name = view::ThemeName::ALL[index];
                view::set_theme(name);
                let note = match crate::config::save_theme(name.slug()) {
                    Ok(_) => format!("theme set to {}", name.label()),
                    Err(err) => format!("theme set to {} (not saved: {err})", name.label()),
                };
                After::CloseWithNote(note)
            }
            _ => After::Nothing,
        },
        Overlay::Views { picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => After::CloseAndSetView(ViewMode::ALL[index]),
            _ => After::Nothing,
        },
        Overlay::TranscriptSpacing { picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => {
                After::CloseAndSetTranscriptSpacing(TranscriptSpacing::ALL[index])
            }
            _ => After::Nothing,
        },
        Overlay::Usage => match key.code {
            KeyCode::Enter | KeyCode::Char('q') => After::Close,
            _ => After::Nothing,
        },
        Overlay::ApiKey { provider, input } => match key.code {
            KeyCode::Enter => {
                let key = input.trim().to_string();
                if key.is_empty() {
                    After::Nothing
                } else {
                    let provider = *provider;
                    let note = match crate::config::save_key(provider.label(), &key) {
                        Ok(path) => format!("api key saved to {}", path.display()),
                        Err(err) => {
                            format!("api key kept for this session only (save failed: {err})")
                        }
                    };
                    After::SendWithNote(
                        WorkerCmd::SetProvider {
                            provider,
                            api_key: Some(key),
                        },
                        note,
                    )
                }
            }
            KeyCode::Char(c) => {
                input.push(c);
                After::Nothing
            }
            KeyCode::Backspace => {
                input.pop();
                After::Nothing
            }
            _ => After::Nothing,
        },
        Overlay::Settings { picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(row) => match row {
                0 => {
                    let selected = Provider::ALL
                        .iter()
                        .position(|p| *p == current_provider)
                        .unwrap_or(0);
                    After::Replace(Overlay::Providers {
                        picker: ListPicker::with_selected(Provider::ALL.len(), selected),
                    })
                }
                1 => After::FetchModels,
                2 => {
                    let current = view::theme_name();
                    let selected = view::ThemeName::ALL
                        .iter()
                        .position(|name| *name == current)
                        .unwrap_or(0);
                    After::Replace(Overlay::Themes {
                        picker: ListPicker::with_selected(view::ThemeName::ALL.len(), selected),
                    })
                }
                3 => {
                    let selected = ViewMode::ALL
                        .iter()
                        .position(|mode| *mode == current_view)
                        .unwrap_or(0);
                    After::Replace(Overlay::Views {
                        picker: ListPicker::with_selected(ViewMode::ALL.len(), selected),
                    })
                }
                4 => {
                    if current_provider.key_env().is_none() {
                        After::CloseWithNote(format!(
                            "the {} endpoint needs no api key",
                            current_provider.label()
                        ))
                    } else {
                        After::Replace(Overlay::ApiKey {
                            provider: current_provider,
                            input: String::new(),
                        })
                    }
                }
                5 => {
                    let tools = crate::config::stored_approvals(&workspace_root);
                    if tools.is_empty() {
                        After::CloseWithNote("no saved approvals for this workspace".into())
                    } else {
                        After::Replace(Overlay::Approvals {
                            picker: ListPicker::new(tools.len()),
                            tools,
                        })
                    }
                }
                _ => {
                    let selected = TranscriptSpacing::ALL
                        .iter()
                        .position(|spacing| *spacing == current_spacing)
                        .unwrap_or(1);
                    After::Replace(Overlay::TranscriptSpacing {
                        picker: ListPicker::with_selected(TranscriptSpacing::ALL.len(), selected),
                    })
                }
            },
            _ => After::Nothing,
        },
        Overlay::Approvals { tools, picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => {
                let tool = tools[index].clone();
                match crate::config::remove_approval(&workspace_root, &tool) {
                    Ok(_) => {
                        tools.retain(|t| *t != tool);
                        picker.set_len(tools.len());
                        if tools.is_empty() {
                            After::CloseWithNote(
                                "all saved approvals removed; these tools ask again".into(),
                            )
                        } else {
                            After::Nothing
                        }
                    }
                    Err(err) => After::CloseWithNote(format!("could not update the config: {err}")),
                }
            }
            _ => After::Nothing,
        },
        Overlay::Extensions { picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => {
                let spec = &crate::extensions::EXTENSIONS[index];
                let enabled = !crate::extensions::is_enabled(spec);
                match crate::config::save_extension(spec.name, enabled) {
                    // Stay open so several extensions can be toggled;
                    // the row re-renders with its new state.
                    Ok(_) => After::Send(WorkerCmd::ReloadExtensions),
                    Err(err) => After::CloseWithNote(format!("could not update the config: {err}")),
                }
            }
            _ => After::Nothing,
        },
        Overlay::Mcp {
            servers,
            filter,
            picker,
        } => {
            let indices = matching_indices(servers, filter, |server| &server.name);
            // Space toggles rather than arming an action strip: this
            // picker has exactly one action, so the strip would be a
            // keystroke of ceremony. Enter does the same, matching
            // /extensions.
            let row = match key.code {
                KeyCode::Char(' ') | KeyCode::Enter if !indices.is_empty() => {
                    Some(indices[picker.index()])
                }
                _ => match picker.on_key(key.code) {
                    PickerEvent::Activated(index) => Some(indices[index]),
                    PickerEvent::Ignored => {
                        match key.code {
                            KeyCode::Char(c) if c != ' ' => filter.push(c),
                            KeyCode::Backspace => {
                                filter.pop();
                            }
                            _ => {}
                        }
                        picker.set_len(
                            matching_indices(servers, filter, |server| &server.name).len(),
                        );
                        return;
                    }
                    _ => None,
                },
            };
            match row {
                Some(index) => {
                    let server = &mut servers[index];
                    let enabled = !server.enabled;
                    match crate::config::set_mcp_enabled(&server.name, enabled) {
                        // Stay open so several servers can be toggled;
                        // the row redraws from this copy at once while
                        // the reconnect runs behind the overlay.
                        Ok(_) => {
                            server.enabled = enabled;
                            After::Send(WorkerCmd::ReloadMcp)
                        }
                        Err(err) => {
                            After::CloseWithNote(format!("could not update the config: {err}"))
                        }
                    }
                }
                None => After::Nothing,
            }
        }
        Overlay::Skills {
            entries,
            filter,
            picker,
        } => match picker.on_key(key.code) {
            // Space reveals the action strip; enter keeps the toggle one
            // key away, since that is what the list is mostly for.
            // Deleting sits behind the strip on purpose: it is the only
            // action here that touches the filesystem. It also needs
            // `app` — the shared handle, the config, the transcript —
            // which this match holds borrowed, so it is handed to the
            // apply step below.
            PickerEvent::Action { key: 'd', row } => {
                let indices = matching_indices(entries, filter, |entry| &entry.name);
                After::RemoveSkill(entries[indices[row]].name.clone())
            }
            event => {
                let row = match event {
                    PickerEvent::Activated(index)
                    | PickerEvent::Action {
                        key: 't',
                        row: index,
                    } => {
                        let indices = matching_indices(entries, filter, |entry| &entry.name);
                        Some(indices[index])
                    }
                    PickerEvent::Ignored => {
                        match key.code {
                            KeyCode::Char(c) if c != ' ' => filter.push(c),
                            KeyCode::Backspace => {
                                filter.pop();
                            }
                            _ => {}
                        }
                        picker
                            .set_len(matching_indices(entries, filter, |entry| &entry.name).len());
                        None
                    }
                    _ => None,
                };
                match row {
                    Some(index) => {
                        let entry = &mut entries[index];
                        match &entry.state {
                            // A row that never loaded has nothing to
                            // switch on; saying why beats a toggle that
                            // does nothing.
                            crate::skills::SkillState::Failed { reason, .. } => {
                                After::Note(format!("skill {} did not load: {reason}", entry.name))
                            }
                            crate::skills::SkillState::Shadowed { root, by } => {
                                After::Note(format!(
                                    "skill {} in {root} is shadowed by the copy in {by} — \
                                     rename it to use both",
                                    entry.name
                                ))
                            }
                            crate::skills::SkillState::Loaded { .. } => {
                                let enabled = !entry.enabled;
                                match crate::config::save_skill_enabled(&entry.name, enabled) {
                                    // Stay open so several can be
                                    // toggled; the row redraws from this
                                    // copy at once while the rescan runs
                                    // behind it.
                                    Ok(_) => {
                                        entry.enabled = enabled;
                                        After::Send(WorkerCmd::ReloadSkills)
                                    }
                                    Err(err) => After::CloseWithNote(format!(
                                        "could not update the config: {err}"
                                    )),
                                }
                            }
                        }
                    }
                    None => After::Nothing,
                }
            }
        },
        Overlay::Sessions { sessions, picker } => {
            // The picker only shows the last few sessions, but the
            // navigation grammar matches /models: ↑↓ step, PgUp/PgDn
            // page through the list.
            match key.code {
                KeyCode::PageUp => {
                    picker.move_by(-(PICKER_ROWS as isize));
                    After::Nothing
                }
                KeyCode::PageDown => {
                    picker.move_by(PICKER_ROWS as isize);
                    After::Nothing
                }
                _ => match picker.on_key(key.code) {
                    PickerEvent::Activated(index) => After::CloseAndSend(WorkerCmd::LoadSession {
                        path: sessions[index].path.clone(),
                    }),
                    PickerEvent::Action { key: 'd', row } => {
                        let session = &sessions[row];
                        if current_session.as_deref() == Some(session.meta.id.as_str()) {
                            After::Note(
                                "the active session cannot be deleted (use /clear to empty it)"
                                    .into(),
                            )
                        } else {
                            match std::fs::remove_file(&session.path) {
                                Ok(()) => {
                                    let id = sessions.remove(row).meta.id;
                                    picker.set_len(sessions.len());
                                    if sessions.is_empty() {
                                        After::CloseWithNote(format!("deleted session {id}"))
                                    } else {
                                        After::Note(format!("deleted session {id}"))
                                    }
                                }
                                Err(err) => After::Note(format!("could not delete: {err}")),
                            }
                        }
                    }
                    _ => After::Nothing,
                },
            }
        }
    };
    match after {
        After::Nothing => {}
        After::Close => app.overlay = None,
        After::Replace(next) => app.overlay = Some(next),
        After::Send(cmd) => send_or_report(app, worker, cmd),
        After::CloseAndSend(cmd) => {
            app.overlay = None;
            send_or_report(app, worker, cmd);
        }
        After::CloseAndSetModel { id, window } => {
            app.overlay = None;
            app.context_window = window;
            send_or_report(app, worker, WorkerCmd::SetModel { id });
        }
        After::CloseAndSetView(mode) => {
            app.overlay = None;
            app.view_mode = mode;
            if mode == ViewMode::Classic {
                clear_tool_connectors(&mut app.transcript);
                clear_tool_connectors(&mut app.pending_history);
                app.split_inspector_cache = None;
            }
            app.split_focused = false;
            app.split_scroll = 0;
            let note = match crate::config::save_view(mode.slug()) {
                Ok(_) => format!("view set to {}", mode.label()),
                Err(err) => format!("view set to {} (not saved: {err})", mode.label()),
            };
            push_notice(app, note);
        }
        After::CloseAndSetTranscriptSpacing(spacing) => {
            app.overlay = None;
            set_transcript_spacing(spacing);
            let note = match crate::config::save_transcript_spacing(spacing.slug()) {
                Ok(_) => format!("transcript spacing set to {}", spacing.label()),
                Err(err) => format!(
                    "transcript spacing set to {} (not saved: {err})",
                    spacing.label()
                ),
            };
            push_notice(app, note);
        }
        After::CloseWithNote(note) => {
            app.overlay = None;
            push_notice(app, note);
        }
        After::Note(note) => {
            push_notice(app, note);
        }
        After::SendWithNote(cmd, note) => {
            app.overlay = None;
            send_or_report(app, worker, cmd);
            push_notice(app, note);
        }
        After::RemoveSkill(name) => {
            // The rescan that follows is the worker's, and it lands
            // later; drop the row now so the list matches what the user
            // just did. An unremovable skill keeps its row and says why.
            if remove_skill(app, &name, worker) {
                if let Some(Overlay::Skills {
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
        }
        After::FetchModels => {
            app.overlay = None;
            app.picker_pending = Some(String::new());
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
        }
        After::InsertLocation { token_start, entry } => {
            let start_byte = byte_index(&app.composer, token_start);
            let end_byte = byte_index(&app.composer, app.cursor);
            let suffix = if entry.directory { "/" } else { "" };
            let mention = format!("@{}{suffix} ", entry.path);
            app.composer.replace_range(start_byte..end_byte, &mention);
            app.cursor = token_start + mention.chars().count();
            app.overlay = None;
        }
    }
}

pub(crate) fn send_or_report(
    app: &mut App,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    cmd: WorkerCmd,
) {
    if worker.send(cmd).is_err() {
        push_error(app, "worker is gone; restart orcacode");
    }
}

/// Move the palette selection by `delta` rows, clamped to the filtered
/// list. Arrow keys, page keys, and the wheel all go through here so
/// they cannot disagree about the bounds; the rendered window follows
/// the selection, so this is what scrolling the list means.
pub(crate) fn palette_move(app: &mut App, delta: isize) {
    let Some(query) = app.palette_query() else {
        return;
    };
    let Some(last) = filter_commands(query).len().checked_sub(1) else {
        return;
    };
    let current = app.palette_index.min(last) as isize;
    app.palette_index = current.saturating_add(delta).clamp(0, last as isize) as usize;
}

/// The highlighted palette entry, if the palette is open and non-empty.
pub(crate) fn palette_selection(app: &App) -> Option<&'static CommandSpec> {
    let query = app.palette_query()?;
    let filtered = filter_commands(query);
    filtered
        .get(app.palette_index.min(filtered.len().saturating_sub(1)))
        .copied()
}

/// A compact transcript notification: the dot keeps system status scannable
/// without giving it the visual weight of transcript content.
pub(crate) fn handle_approval_key(app: &mut App, key: KeyEvent) {
    let response = match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(ApprovalResponse::AllowOnce),
        KeyCode::Char('a') => Some(ApprovalResponse::AllowAlways),
        // Deliberately a distinct key: persisting trust across sessions
        // must never happen from a habitual lowercase 'a'.
        KeyCode::Char('A') => Some(ApprovalResponse::AllowAlwaysSave),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(ApprovalResponse::Deny),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(ApprovalResponse::Deny)
        }
        _ => None,
    };
    if let Some(response) = response {
        if let Some(request) = app.approval.take() {
            let verdict = match response {
                ApprovalResponse::AllowOnce => "approved",
                ApprovalResponse::AllowAlways => "always allowed",
                ApprovalResponse::AllowAlwaysSave => "always allowed (saved)",
                ApprovalResponse::Deny => "denied",
            };
            if response == ApprovalResponse::AllowAlwaysSave {
                let note =
                    match crate::config::save_approval(&app.cfg.workspace_root, &request.tool_name)
                    {
                        Ok(_) => format!(
                            "{} always allowed in this workspace — saved; /settings to revoke",
                            request.tool_name
                        ),
                        Err(err) => format!(
                            "{} always allowed this session only (save failed: {err})",
                            request.tool_name
                        ),
                    };
                push_notice(app, note);
            }
            if let Some(activity) = app.activity_tools.iter_mut().rev().find(|activity| {
                activity.tool_name == request.tool_name && activity.output.is_none()
            }) {
                activity.approval = Some(verdict.to_string());
            }
            let _ = request.respond.send(response);
        }
    }
}

pub(crate) fn history_nav(app: &mut App, dir: i32) {
    if app.prompt_history.is_empty() {
        return;
    }
    let last = app.prompt_history.len() - 1;
    let next = match (app.history_pos, dir) {
        (None, -1) => Some(last),
        (None, _) => None,
        (Some(0), -1) => Some(0),
        (Some(p), -1) => Some(p - 1),
        (Some(p), 1) if p >= last => None,
        (Some(p), 1) => Some(p + 1),
        (pos, _) => pos,
    };
    app.history_pos = next;
    app.composer = next
        .map(|p| app.prompt_history[p].clone())
        .unwrap_or_default();
    app.cursor = app.composer.chars().count();
}
