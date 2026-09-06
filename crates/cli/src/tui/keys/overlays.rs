use super::effects::After;
use super::*;
mod apply;
use apply::apply_after;

pub(crate) fn handle_overlay_key(
    app: &mut App,
    key: KeyEvent,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c')) {
        app.overlay = None;
        app.overlay_stack.clear();
        return;
    }
    if key.code == KeyCode::Left {
        if let Some(parent) = app.overlay_stack.pop() {
            app.overlay = Some(parent);
            return;
        }
    }
    // In-place edits happen under the borrow; anything that replaces the
    // overlay or talks to the worker is deferred until the borrow ends.
    // Side effects are represented separately so overlay matching only
    // decides what should happen after its mutable borrow ends.
    // Read before the overlay borrow: the settings rows need these.
    let current_provider = app.cfg.provider;
    let current_view = app.view_mode;
    let current_inspector = app.inspector_mode;
    let current_spacing = transcript_spacing();
    let current_style = ui_style();
    let workspace_root = app.cfg.workspace_root.clone();
    let current_session = app.cfg.session_id.clone();
    let Some(overlay) = app.overlay.as_mut() else {
        return;
    };
    let after = match overlay {
        Overlay::Help { filter, picker } => {
            super::catalogs::handle_help_key(filter, picker, key.code)
        }
        Overlay::Models(picker) => super::catalogs::handle_model_key(picker, key.code),
        Overlay::Efforts(picker) => super::catalogs::handle_effort_key(picker, key.code),
        Overlay::Locations(location) => super::location_mentions::handle_location_mention_key(
            location,
            &mut app.composer,
            &mut app.cursor,
            key.code,
        ),
        Overlay::SkillMentions(skill) => super::skill_mentions::handle_skill_mention_key(
            skill,
            &mut app.composer,
            &mut app.cursor,
            key.code,
        ),
        Overlay::Providers { picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => {
                let provider = Provider::ALL[index];
                match provider.auth() {
                    ProviderAuth::ApiKey { .. } if provider.resolve_key().is_none() => {
                        After::Push(Overlay::ApiKey {
                            provider,
                            input: String::new(),
                        })
                    }
                    ProviderAuth::OAuth => match crate::auth::status(provider) {
                        Ok(_) => After::CloseAndSend(WorkerCmd::SetProvider {
                            provider,
                            api_key: None,
                        }),
                        Err(_) => After::CloseAndSend(WorkerCmd::LoginProvider { provider }),
                    },
                    _ => After::CloseAndSend(WorkerCmd::SetProvider {
                        provider,
                        api_key: None,
                    }),
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
        Overlay::Mode { picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => After::CloseAndSetMode(crate::mode::Mode::ALL[index]),
            _ => After::Nothing,
        },
        Overlay::TranscriptSpacing { picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => {
                After::CloseAndSetTranscriptSpacing(TranscriptSpacing::ALL[index])
            }
            _ => After::Nothing,
        },
        Overlay::Style { picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => After::CloseAndSetStyle(UiStyle::ALL[index]),
            _ => After::Nothing,
        },
        Overlay::Inspector { picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => After::CloseAndSetInspector(InspectorMode::ALL[index]),
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
                    After::Push(Overlay::Providers {
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
                    After::Push(Overlay::Themes {
                        picker: ListPicker::with_selected(view::ThemeName::ALL.len(), selected),
                    })
                }
                3 => {
                    let selected = ViewMode::ALL
                        .iter()
                        .position(|mode| *mode == current_view)
                        .unwrap_or(0);
                    After::Push(Overlay::Views {
                        picker: ListPicker::with_selected(ViewMode::ALL.len(), selected),
                    })
                }
                4 => {
                    let selected = InspectorMode::ALL
                        .iter()
                        .position(|mode| *mode == current_inspector)
                        .unwrap_or(0);
                    After::Push(Overlay::Inspector {
                        picker: ListPicker::with_selected(InspectorMode::ALL.len(), selected),
                    })
                }
                5 => {
                    if current_provider.key_env().is_none() {
                        After::CloseWithNote(format!(
                            "the {} endpoint needs no api key",
                            current_provider.label()
                        ))
                    } else {
                        After::Push(Overlay::ApiKey {
                            provider: current_provider,
                            input: String::new(),
                        })
                    }
                }
                6 => {
                    let tools = crate::config::stored_approvals(&workspace_root);
                    if tools.is_empty() {
                        After::CloseWithNote("no saved approvals for this workspace".into())
                    } else {
                        After::Push(Overlay::Approvals {
                            picker: ListPicker::new(tools.len()),
                            tools,
                        })
                    }
                }
                7 => {
                    let selected = TranscriptSpacing::ALL
                        .iter()
                        .position(|spacing| *spacing == current_spacing)
                        .unwrap_or(1);
                    After::Push(Overlay::TranscriptSpacing {
                        picker: ListPicker::with_selected(TranscriptSpacing::ALL.len(), selected),
                    })
                }
                _ => {
                    let selected = UiStyle::ALL
                        .iter()
                        .position(|style| *style == current_style)
                        .unwrap_or(0);
                    After::Push(Overlay::Style {
                        picker: ListPicker::with_selected(UiStyle::ALL.len(), selected),
                    })
                }
            },
            _ => After::Nothing,
        },
        Overlay::Subagents { picker } => match picker.on_key(key.code) {
            PickerEvent::Activated(row) => {
                let setting = SubagentSetting::ALL[row];
                let values = subagent_values(&app.cfg.subagent_depth, setting);
                let selected = subagent_selected(&app.cfg.subagent_depth, setting, &values);
                After::Push(Overlay::SubagentValues {
                    setting,
                    picker: ListPicker::with_selected(values.len(), selected),
                    values,
                })
            }
            _ => After::Nothing,
        },
        Overlay::SubagentValues {
            setting,
            values,
            picker,
        } => match picker.on_key(key.code) {
            PickerEvent::Activated(index) => {
                let value = values[index].clone();
                if setting.numeric().is_some() && value == crate::tui::subagents::CUSTOM_VALUE {
                    let setting = *setting;
                    return apply_after(
                        app,
                        worker,
                        After::Replace(Overlay::SubagentNumber {
                            setting,
                            input: String::new(),
                            error: String::new(),
                        }),
                    );
                }
                if let Some(tier) = crate::tui::subagents::tier(*setting) {
                    if let Some(provider) = crate::Provider::from_label(&value) {
                        let after = After::FetchSubagentModels {
                            tier: tier.into(),
                            provider,
                        };
                        return apply_after(app, worker, after);
                    }
                }
                apply_subagent_value(&app.cfg.subagent_depth, *setting, &value);
                let note =
                    crate::tui::subagents::save_subagent_note(&app.cfg.subagent_depth, *setting);
                After::PopWithNote(note)
            }
            _ => After::Nothing,
        },
        Overlay::SubagentNumber {
            setting,
            input,
            error,
        } => match key.code {
            KeyCode::Char('u') if ctrl => {
                input.clear();
                error.clear();
                After::Nothing
            }
            KeyCode::Char(c) if !ctrl => {
                input.push(c);
                error.clear();
                After::Nothing
            }
            KeyCode::Backspace => {
                input.pop();
                error.clear();
                After::Nothing
            }
            KeyCode::Enter => {
                let field = setting.numeric().expect("numeric editor");
                match field.parse(input) {
                    Ok(value) => {
                        (field.write)(&app.cfg.subagent_depth, value);
                        After::PopWithNote(crate::tui::subagents::save_subagent_note(
                            &app.cfg.subagent_depth,
                            *setting,
                        ))
                    }
                    Err(message) => {
                        *error = message.into();
                        After::Nothing
                    }
                }
            }
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
            entries,
            filter,
            picker,
        } => {
            let indices = crate::tui::mcp_picker::matching_indices(entries, filter);
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
                            crate::tui::mcp_picker::matching_indices(entries, filter).len(),
                        );
                        return;
                    }
                    _ => None,
                },
            };
            match row {
                Some(index) => match &mut entries[index] {
                    crate::tui::mcp_picker::Entry::Standalone(server) => {
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
                    crate::tui::mcp_picker::Entry::Plugin(server) => After::CloseWithNote(format!(
                        "plugin MCP {}/{} is read-only here; manage it with /plugin",
                        server.plugin, server.server
                    )),
                },
                None => After::Nothing,
            }
        }
        Overlay::Plugins {
            entries,
            filter,
            picker,
        } => match picker.on_key(key.code) {
            event @ (PickerEvent::Activated(_) | PickerEvent::Action { .. }) => {
                let (row, action) = match event {
                    PickerEvent::Activated(row) => (row, 't'),
                    PickerEvent::Action { key, row } => (row, key),
                    _ => unreachable!(),
                };
                let indices = matching_indices(entries, filter, |entry| &entry.name);
                After::PluginAction {
                    plugin: entries[indices[row]].clone(),
                    action,
                }
            }
            PickerEvent::Ignored => {
                match key.code {
                    KeyCode::Char(c) if c != ' ' => filter.push(c),
                    KeyCode::Backspace => {
                        filter.pop();
                    }
                    _ => {}
                }
                picker.set_len(matching_indices(entries, filter, |entry| &entry.name).len());
                After::Nothing
            }
            _ => After::Nothing,
        },
        Overlay::Skills {
            entries,
            filter,
            picker,
        } => match picker.on_key(key.code) {
            // Space reveals the action strip; enter toggles a standalone
            // row or inserts a read-only plugin Skill mention.
            // Deleting sits behind the strip on purpose: it is the only
            // action here that touches the filesystem. It also needs
            // `app` — the shared handle, the config, the transcript —
            // which this match holds borrowed, so it is handed to the
            // apply step below.
            PickerEvent::Action { key: 'd', row } => {
                let indices = matching_indices(entries, filter, |entry| &entry.name);
                let entry = &entries[indices[row]];
                match crate::tui::skills_picker::plugin_name(entry) {
                    Some(plugin) => After::CloseWithNote(format!(
                        "plugin Skill ${} from {plugin} is read-only here; manage it with /plugin",
                        entry.name
                    )),
                    None => After::RemoveSkill(entry.name.clone()),
                }
            }
            event => {
                let row = match event {
                    PickerEvent::Activated(index) => {
                        let indices = matching_indices(entries, filter, |entry| &entry.name);
                        Some((indices[index], true))
                    }
                    PickerEvent::Action {
                        key: 't',
                        row: index,
                    } => {
                        let indices = matching_indices(entries, filter, |entry| &entry.name);
                        Some((indices[index], false))
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
                    Some((index, activated)) => {
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
                                if let Some(plugin) = crate::tui::skills_picker::plugin_name(entry)
                                {
                                    if activated {
                                        After::CloseAndCompose(format!("${} ", entry.name))
                                    } else {
                                        After::CloseWithNote(format!(
                                                "plugin Skill ${} from {plugin} is read-only here; manage it with /plugin",
                                                entry.name
                                            ))
                                    }
                                } else {
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
                                "the active session cannot be deleted (use /clear to preserve it and start fresh)"
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
    apply_after(app, worker, after);
}
