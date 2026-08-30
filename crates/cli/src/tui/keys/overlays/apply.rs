use super::*;

fn finish_picker_flow(app: &mut App) {
    app.overlay = None;
    app.overlay_stack.clear();
}

pub(super) fn apply_after(app: &mut App, worker: &mpsc::UnboundedSender<WorkerCmd>, after: After) {
    match after {
        After::Nothing => {}
        After::Close => finish_picker_flow(app),
        After::CloseAndCompose(command) => {
            finish_picker_flow(app);
            app.composer = command;
            app.cursor = app.composer.chars().count();
            app.reset_palette_picker();
        }
        After::Push(next) => {
            if let Some(current) = app.overlay.replace(next) {
                app.overlay_stack.push(current);
            }
        }
        After::Send(cmd) => send_or_report(app, worker, cmd),
        After::CloseAndSend(cmd) => {
            finish_picker_flow(app);
            send_or_report(app, worker, cmd);
        }
        After::CloseAndSetModel {
            id,
            window,
            reasoning_effort,
        } => {
            finish_picker_flow(app);
            app.context_window = window;
            send_or_report(
                app,
                worker,
                WorkerCmd::SetModel {
                    id,
                    reasoning_effort,
                },
            );
        }
        After::CloseAndSetMode(mode) => {
            finish_picker_flow(app);
            crate::tui::commands::apply_mode(app, mode);
        }
        After::CloseAndSetView(mode) => {
            finish_picker_flow(app);
            app.view_mode = mode;
            if mode == ViewMode::Classic {
                clear_tool_connectors(&mut app.transcript);
                clear_tool_connectors(&mut app.pending_history);
                app.split_inspector_cache = None;
            }
            app.split_scroll = 0;
            let note = match crate::config::save_view(mode.slug()) {
                Ok(_) => format!("view set to {}", mode.label()),
                Err(err) => format!("view set to {} (not saved: {err})", mode.label()),
            };
            push_notice(app, note);
        }
        After::CloseAndSetTranscriptSpacing(spacing) => {
            finish_picker_flow(app);
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
        After::CloseAndSetInspector(mode) => {
            finish_picker_flow(app);
            app.inspector_mode = mode;
            app.split_inspector_cache = None;
            app.split_scroll = 0;
            let note = match crate::config::save_inspector(mode.slug()) {
                Ok(_) => format!("Tool Inspector set to {}", mode.label()),
                Err(err) => format!("Tool Inspector set to {} (not saved: {err})", mode.label()),
            };
            push_notice(app, note);
        }
        After::CloseWithNote(note) => {
            finish_picker_flow(app);
            push_notice(app, note);
        }
        After::PopWithNote(note) => {
            app.overlay = app.overlay_stack.pop();
            push_notice(app, note);
        }
        After::Note(note) => {
            push_notice(app, note);
        }
        After::SendWithNote(cmd, note) => {
            finish_picker_flow(app);
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
        After::PluginAction { plugin, action } => {
            plugin_picker_action(app, worker, plugin, action);
        }
        After::FetchModels => {
            if let Some(current) = app.overlay.take() {
                app.overlay_stack.push(current);
            }
            let request_id = app.next_picker_request;
            app.next_picker_request += 1;
            app.picker_pending = Some((request_id, String::new()));
            if worker
                .send(WorkerCmd::ListModels {
                    request_id,
                    filter: String::new(),
                })
                .is_err()
            {
                app.picker_pending = None;
                app.overlay = app.overlay_stack.pop();
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
        After::InsertSkill { token_start, name } => {
            let start_byte = byte_index(&app.composer, token_start);
            let end_byte = byte_index(&app.composer, app.cursor);
            let mention = format!("${name} ");
            app.composer.replace_range(start_byte..end_byte, &mention);
            app.cursor = token_start + mention.chars().count();
            app.overlay = None;
        }
    }
}
