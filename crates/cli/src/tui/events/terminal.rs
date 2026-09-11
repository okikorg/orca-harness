// Raw crossterm input normalization and composer/global key dispatch.

use std::io::{self, Write};
use std::path::Path;

use crossterm::event::{Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use tokio::sync::mpsc;

use crate::msg::{ApprovalResponse, WorkerCmd};
use crate::tui::clipboard;
use crate::tui::components::ask::AskFormEvent;
use crate::tui::components::picker::ListPicker;

use super::super::composer::{
    mention_starts_at, remove_location_mention_before_cursor, remove_skill_mention_before_cursor,
    workspace_locations,
};
use super::super::format::byte_index;
use super::super::input::{
    insert_clipboard_image, insert_paste, marker_ending_at, marker_starting_at, remove_marker,
};
use super::super::render::{
    agent_ids, cycle_agent_tab, selected_agent_copy, transcript_content_width,
};
use super::super::state::{
    AgentBrowser, App, LocationPicker, Overlay, RunState, SkillMentionPicker, StatusFocus,
};
use super::super::PALETTE_ROWS;
use super::super::{
    copy_command, expand_latest_work, expand_tool, handle_approval_key, handle_overlay_key,
    history_nav, palette_move, palette_selection, scroll_transcript, submit, SCROLL_PAGE,
};

pub(crate) fn flush_terminal_requests(app: &mut App) -> io::Result<()> {
    let mut out = io::stdout();
    if let Some(text) = app.clipboard_pending.take() {
        out.write_all(clipboard::osc52(&text).as_bytes())?;
        out.flush()?;
    }
    Ok(())
}
pub(crate) fn handle_terminal_event(
    app: &mut App,
    event: CtEvent,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    width: usize,
) {
    let key = match event {
        CtEvent::Key(key) => key,
        CtEvent::Mouse(mouse) => {
            if let Some(browser) = &mut app.agent_browser {
                match mouse.kind {
                    MouseEventKind::ScrollUp => browser.scroll = browser.scroll.saturating_add(3),
                    MouseEventKind::ScrollDown => browser.scroll = browser.scroll.saturating_sub(3),
                    _ => {}
                }
                return;
            }
            let over_inspector = app.inspector_area.is_some_and(|area| {
                area.contains(ratatui::layout::Position::new(mouse.column, mouse.row))
            });
            match mouse.kind {
                MouseEventKind::ScrollUp if over_inspector => {
                    app.split_scroll = app.split_scroll.saturating_sub(3)
                }
                MouseEventKind::ScrollDown if over_inspector => {
                    app.split_scroll = app.split_scroll.saturating_add(3)
                }
                // An open palette is a list, not part of the transcript:
                // the wheel moves its selection rather than scrolling
                // the conversation out from under it.
                MouseEventKind::ScrollUp if app.palette_query().is_some() => palette_move(app, -1),
                MouseEventKind::ScrollDown if app.palette_query().is_some() => palette_move(app, 1),
                MouseEventKind::ScrollUp => scroll_transcript(app, 3),
                MouseEventKind::ScrollDown => scroll_transcript(app, -3),
                _ => {}
            }
            return;
        }
        // Most overlays are keystroke menus with nowhere to put pasted text.
        // Text-entry overlays accept bracketed paste like typed characters.
        CtEvent::Paste(text) => {
            if let Some(ask) = app.ask.as_mut() {
                ask.handle_paste(&text);
            } else if let Some(Overlay::SubagentNumber { input, error, .. }) = app.overlay.as_mut()
            {
                input.push_str(&text);
                error.clear();
            } else if let Some(Overlay::ApiKey { input, .. }) = app.overlay.as_mut() {
                input.push_str(&text);
            } else if app.approval.is_none() && app.overlay.is_none() && app.agent_browser.is_none()
            {
                insert_paste(app, &text);
            }
            return;
        }
        _ => return,
    };
    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
        return;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // Ctrl+Tab has no Orca behavior, including while a modal that uses plain
    // Tab is open. Consume it before those handlers can discard modifiers.
    if ctrl && key.code == KeyCode::Tab {
        return;
    }
    if let Some(request) = &app.approval {
        let plan = request.tool_name == crate::tui::commands::PLAN_APPROVAL;
        if handle_approval_key(app, key) == Some(ApprovalResponse::AllowOnce) && plan {
            crate::tui::commands::approve_plan(app, worker, width);
        }
        return;
    }
    if app.ask.is_some() {
        let event = app.ask.as_mut().expect("ask checked").handle_key(key);
        if matches!(event, AskFormEvent::Submit | AskFormEvent::Cancel) {
            app.ask.take().expect("ask present").finish(event);
        }
        return;
    }
    if app.agent_browser.is_some() {
        match key.code {
            KeyCode::Esc | KeyCode::Left => app.agent_browser = None,
            KeyCode::Tab => cycle_agent_tab(app),
            KeyCode::Char('y') if ctrl => {
                if let Some(text) = selected_agent_copy(app) {
                    app.clipboard_pending = Some(text);
                }
            }
            KeyCode::PageUp => {
                if let Some(browser) = &mut app.agent_browser {
                    browser.scroll = browser.scroll.saturating_add(PALETTE_ROWS);
                }
            }
            KeyCode::PageDown => {
                if let Some(browser) = &mut app.agent_browser {
                    browser.scroll = browser.scroll.saturating_sub(PALETTE_ROWS);
                }
            }
            KeyCode::Up | KeyCode::Down => {
                if let Some(browser) = &mut app.agent_browser {
                    let previous = browser.picker.index();
                    browser.picker.on_key(key.code);
                    if browser.picker.index() != previous {
                        browser.scroll = 0;
                    }
                }
            }
            _ => {}
        }
        return;
    }
    if app.overlay.is_some() {
        handle_overlay_key(app, key, worker);
        return;
    }
    if app.picker_pending.is_some() {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Left && !app.overlay_stack.is_empty() {
            app.picker_pending = None;
            app.overlay = app.overlay_stack.pop();
            return;
        }
        if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c')) {
            app.picker_pending = None;
            app.overlay_stack.clear();
            return;
        }
        if !app.overlay_stack.is_empty() {
            return;
        }
    }
    if let Some(focus) = app.status_focus {
        let items = status_items(app);
        let index = items.iter().position(|item| *item == focus).unwrap_or(0);
        match key.code {
            KeyCode::Enter => {
                match focus {
                    StatusFocus::Context => app.overlay = Some(Overlay::Usage),
                    StatusFocus::Processes => app.overlay = Some(Overlay::Processes),
                    StatusFocus::Agents => {
                        app.agent_browser = Some(AgentBrowser::new(agent_ids(app).len()))
                    }
                    StatusFocus::Todo => app.overlay = Some(Overlay::Todo),
                }
                app.status_focus = None;
                return;
            }
            KeyCode::Up | KeyCode::Esc => {
                app.status_focus = None;
                return;
            }
            KeyCode::Left => {
                app.status_focus = Some(items[index.saturating_sub(1)]);
                return;
            }
            KeyCode::Right => {
                app.status_focus = Some(items[(index + 1).min(items.len() - 1)]);
                return;
            }
            KeyCode::Down => return,
            _ => app.status_focus = None,
        }
    }
    let content_width = transcript_content_width(app, width);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char('c') if ctrl => {
            if let RunState::Running { cancel, .. } = &app.run {
                cancel.cancel();
            } else if !app.composer.is_empty() {
                app.composer.clear();
                app.cursor = 0;
            } else {
                app.quit = true;
            }
        }
        KeyCode::Char('d') if ctrl && app.composer.is_empty() => app.quit = true,
        KeyCode::Char('a') if ctrl => app.cursor = 0,
        KeyCode::Char('e') if ctrl => app.cursor = app.composer.chars().count(),
        KeyCode::Char('u') if ctrl => {
            app.composer.clear();
            app.cursor = 0;
        }
        KeyCode::Char('o') if ctrl => {
            if !expand_latest_work(app) {
                expand_tool(app, 1, content_width);
            }
        }
        // Deliberately not ctrl+s (XOFF on most terminals — the app would
        // appear to hang) and not a plain letter the composer needs.
        KeyCode::Char('y') if ctrl => copy_command(app, ""),
        // Line scroll on shift+arrows: PgUp/PgDn are fn+arrows on a laptop
        // keyboard, which terminals often swallow for their own scrollback
        // before the app ever sees them.
        KeyCode::Up if shift => scroll_transcript(app, 1),
        KeyCode::Down if shift => scroll_transcript(app, -1),
        // Page keys belong to the palette while it is open, for the same
        // reason the wheel does: the list is what the user is looking at.
        KeyCode::PageUp if app.palette_query().is_some() => {
            palette_move(app, -(PALETTE_ROWS as isize))
        }
        KeyCode::PageDown if app.palette_query().is_some() => {
            palette_move(app, PALETTE_ROWS as isize)
        }
        KeyCode::PageUp => scroll_transcript(app, SCROLL_PAGE as isize),
        KeyCode::PageDown => scroll_transcript(app, -(SCROLL_PAGE as isize)),
        KeyCode::BackTab | KeyCode::Tab if shift => {
            let current = app.cfg.mode.get();
            let index = crate::mode::Mode::ALL
                .iter()
                .position(|mode| *mode == current)
                .unwrap_or(0);
            let next = crate::mode::Mode::ALL[(index + 1) % crate::mode::Mode::ALL.len()];
            crate::tui::commands::apply_mode(app, next);
        }
        KeyCode::Esc => {
            if app.palette_query().is_some() {
                app.composer.clear();
                app.cursor = 0;
                app.reset_palette_picker();
            } else if let RunState::Running { cancel, .. } = &app.run {
                cancel.cancel();
            } else {
                app.composer.clear();
                app.cursor = 0;
            }
        }
        KeyCode::Tab => {
            if let Some(spec) = palette_selection(app) {
                app.composer = if spec.takes_args {
                    format!("/{} ", spec.name)
                } else {
                    format!("/{}", spec.name)
                };
                app.cursor = app.composer.chars().count();
            }
        }
        KeyCode::Enter => {
            // Palette open with a selection and no arguments typed:
            // enter uses the highlighted command as-is.
            if let Some(spec) = palette_selection(app) {
                if !app.composer.trim().contains(char::is_whitespace) {
                    app.composer = format!("/{}", spec.name);
                    app.cursor = app.composer.chars().count();
                }
            }
            app.scroll = 0;
            submit(app, worker, content_width);
            app.reset_palette_picker();
        }
        KeyCode::Char('v') if ctrl => {
            if key.kind == KeyEventKind::Press {
                insert_clipboard_image(app);
            }
        }
        KeyCode::Char(c) => {
            let at = byte_index(&app.composer, app.cursor);
            app.composer.insert(at, c);
            app.cursor += 1;
            app.reset_palette_picker();
            if c == '@' && mention_starts_at(&app.composer, app.cursor - 1) {
                let entries = workspace_locations(Path::new(&app.cfg.workspace_root));
                app.overlay_stack.clear();
                app.overlay = Some(Overlay::Locations(LocationPicker {
                    picker: ListPicker::new(entries.len()),
                    entries,
                    query: String::new(),
                    token_start: app.cursor - 1,
                }));
            } else if c == '$'
                && !app.composer.starts_with('!')
                && mention_starts_at(&app.composer, app.cursor - 1)
            {
                let entries = app.cfg.skills.invokable();
                if !entries.is_empty() {
                    app.overlay_stack.clear();
                    app.overlay = Some(Overlay::SkillMentions(SkillMentionPicker {
                        picker: ListPicker::new(entries.len()),
                        entries,
                        query: String::new(),
                        token_start: app.cursor - 1,
                    }));
                }
            }
        }
        KeyCode::Backspace => {
            if let Some((start, end)) = marker_ending_at(&app.pastes, &app.composer, app.cursor) {
                remove_marker(app, start, end);
                app.reset_palette_picker();
            } else if remove_location_mention_before_cursor(&mut app.composer, &mut app.cursor)
                || (!app.composer.starts_with('!')
                    && remove_skill_mention_before_cursor(
                        &mut app.composer,
                        &mut app.cursor,
                        |name| app.cfg.skills.is_invokable(name),
                    ))
            {
                app.reset_palette_picker();
            } else if app.cursor > 0 {
                let at = byte_index(&app.composer, app.cursor - 1);
                app.composer.remove(at);
                app.cursor -= 1;
                app.reset_palette_picker();
            }
        }
        KeyCode::Delete => {
            if let Some((start, end)) = marker_starting_at(&app.pastes, &app.composer, app.cursor) {
                remove_marker(app, start, end);
                app.reset_palette_picker();
            } else if app.cursor < app.composer.chars().count() {
                let at = byte_index(&app.composer, app.cursor);
                app.composer.remove(at);
                app.reset_palette_picker();
            }
        }
        // Arrows step over a marker whole too: landing inside one would
        // let the next keystroke corrupt it into text that no longer
        // expands.
        KeyCode::Left => {
            app.cursor = match marker_ending_at(&app.pastes, &app.composer, app.cursor) {
                Some((start, _)) => start,
                None => app.cursor.saturating_sub(1),
            }
        }
        KeyCode::Right if app.palette_query().is_some() => {
            if let Some(spec) = palette_selection(app) {
                let args = app
                    .composer
                    .find(char::is_whitespace)
                    .map(|at| app.composer[at..].to_string())
                    .unwrap_or_default();
                app.composer = format!("/{}{args}", spec.name);
                app.cursor = app.composer.chars().count();
            }
            app.scroll = 0;
            submit(app, worker, content_width);
            app.reset_palette_picker();
        }
        KeyCode::Right => {
            app.cursor = match marker_starting_at(&app.pastes, &app.composer, app.cursor) {
                Some((_, end)) => end,
                None => (app.cursor + 1).min(app.composer.chars().count()),
            }
        }
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.composer.chars().count(),
        KeyCode::Up => {
            if app.palette_query().is_some() {
                palette_move(app, -1);
            } else {
                history_nav(app, -1);
            }
        }
        KeyCode::Down => {
            if app.palette_query().is_some() {
                palette_move(app, 1);
            } else if app.composer.is_empty() && app.history_pos.is_none() {
                app.status_focus = status_items(app).first().copied();
            } else {
                history_nav(app, 1);
            }
        }
        _ => {}
    }
}

fn status_items(app: &App) -> Vec<StatusFocus> {
    let mut items = vec![StatusFocus::Context];
    if app.cfg.stats.processes() > 0 {
        items.push(StatusFocus::Processes);
    }
    if !app.subagent_transcripts.is_empty() {
        items.push(StatusFocus::Agents);
    }
    if !app.cfg.todos.is_empty() {
        items.push(StatusFocus::Todo);
    }
    items
}
