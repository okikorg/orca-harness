use super::*;

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
    app.palette_picker.set_len(filter_commands(query).len());
    app.palette_picker.move_by(delta);
}

/// The highlighted palette entry, if the palette is open and non-empty.
pub(crate) fn palette_selection(app: &App) -> Option<&'static CommandSpec> {
    let query = app.palette_query()?;
    let filtered = filter_commands(query);
    filtered
        .get(
            app.palette_picker
                .index()
                .min(filtered.len().saturating_sub(1)),
        )
        .copied()
}

/// A compact transcript notification: the dot keeps system status scannable
/// without giving it the visual weight of transcript content.
pub(crate) fn handle_approval_key(app: &mut App, key: KeyEvent) {
    let yes_no = app.approval.as_ref().is_some_and(|request| request.yes_no);
    let response = match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(ApprovalResponse::AllowOnce),
        // Yes/no prompts have no "always": the decision is one-off.
        KeyCode::Char('a') if !yes_no => Some(ApprovalResponse::AllowAlways),
        // Deliberately a distinct key: persisting trust across sessions
        // must never happen from a habitual lowercase 'a'.
        KeyCode::Char('A') if !yes_no => Some(ApprovalResponse::AllowAlwaysSave),
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
