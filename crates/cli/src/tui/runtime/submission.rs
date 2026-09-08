use super::super::*;
use crate::tui::format::estimate_tokens;

pub(crate) fn submit(app: &mut App, worker: &mpsc::UnboundedSender<WorkerCmd>, width: usize) {
    let prompt = app.composer.trim().to_string();
    if prompt.is_empty() {
        if !app.running() {
            start_next_queued_prompt(app, worker, width);
        }
        return;
    }

    if !prompt.starts_with('!')
        && !app.cfg.provider.supports_images()
        && !prompt_images(&app.pastes, &prompt).is_empty()
    {
        push_error(
            app,
            "this model does not support image input; select another model",
        );
        return;
    }

    // Queue management is intentionally available during a run. Other
    // slash commands retain the existing one-run-at-a-time behavior and
    // stay in the composer until the active turn finishes.
    let command = prompt.strip_prefix('/').map(str::trim);
    if app.running()
        && !command.is_some_and(|command| command == "queue" || command.starts_with("queue "))
        && command.is_some()
    {
        return;
    }

    app.composer.clear();
    app.cursor = 0;
    app.history_pos = None;
    app.prompt_history.push(prompt.clone());

    if let Some(command) = command {
        slash_command(app, command, worker, width);
        return;
    }

    if app.running() || !app.prompt_queue.is_empty() {
        app.prompt_queue.push_back(prompt);
        if !app.running() {
            start_next_queued_prompt(app, worker, width);
        }
        return;
    }

    start_submission(app, worker, prompt, width);
}

pub(crate) fn start_submission(
    app: &mut App,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    prompt: String,
    width: usize,
) -> bool {
    if let Some(command) = prompt.strip_prefix('!') {
        let command = expand_pastes(&app.pastes, command.trim());
        start_shell(
            app,
            worker,
            expand_pastes(&app.pastes, &prompt),
            // The picker also fires on `!` lines, and a shell has no use for
            // the marker either.
            strip_location_mentions(command.trim()),
            width,
        )
    } else {
        start_prompt(app, worker, prompt, width)
    }
}

pub(crate) fn start_shell(
    app: &mut App,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    prompt: String,
    command: String,
    width: usize,
) -> bool {
    if command.is_empty() {
        push_notice(app, "usage: !command");
        return false;
    }
    let cancel = CancellationToken::new();
    let id = next_run_id(app);
    if worker
        .send(WorkerCmd::Shell {
            id: id.clone(),
            command,
            working_dir: app.cfg.workspace_root.clone(),
            cancel: cancel.clone(),
        })
        .is_err()
    {
        push_error(app, "worker is gone; restart orcacode");
        return false;
    }
    app.reset_activity();
    if app.turn_count > 0 {
        app.push_line(Line::from(""));
    }
    app.push_user_prompt(&prompt, width);
    app.turn_count += 1;
    app.run = RunState::Running {
        id,
        started: Instant::now(),
        cancel,
    };
    true
}

/// Start one prompt and commit it to the transcript only after the worker
/// accepts it. Returns false when the worker is gone.
pub(crate) fn start_prompt(
    app: &mut App,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    prompt: String,
    width: usize,
) -> bool {
    let cancel = CancellationToken::new();
    let id = next_run_id(app);
    let images = prompt_images(&app.pastes, &prompt);
    // Text paste markers are composer affordances; image pills remain in
    // the visible/model text as references alongside their native payloads.
    let prompt = expand_pastes(&app.pastes, &prompt);
    app.context_tokens += (prompt.len() / 4) as u64;
    let model_prompt = strip_location_mentions(&prompt);
    let prompt_tokens = estimate_tokens(&model_prompt);
    if worker
        .send(WorkerCmd::Run {
            id: id.clone(),
            prompt: model_prompt,
            images,
            cancel: cancel.clone(),
        })
        .is_err()
    {
        push_error(app, "worker is gone; restart orcacode");
        return false;
    }

    app.reset_activity();
    app.turn_tokens_in = prompt_tokens;

    if app.turn_count > 0 {
        app.push_line(Line::from(""));
    }
    app.push_user_prompt(&prompt, width);
    app.turn_count += 1;
    app.run = RunState::Running {
        id,
        started: Instant::now(),
        cancel,
    };
    true
}

fn next_run_id(app: &mut App) -> crate::msg::RunId {
    app.next_run_id = app.next_run_id.wrapping_add(1);
    crate::msg::RunId::User(app.next_run_id)
}

/// Resume the oldest waiting prompt. A failed send leaves the item queued
/// so the UI cannot silently discard work.
pub(crate) fn start_next_queued_prompt(
    app: &mut App,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    width: usize,
) -> bool {
    let Some(prompt) = app.prompt_queue.front().cloned() else {
        return false;
    };
    if !start_submission(app, worker, prompt, width) {
        return false;
    }
    app.prompt_queue.pop_front();
    true
}

/// Print the full output of the n-th most recent tool call (1 = latest)
/// into the transcript.
pub(crate) fn expand_tool(app: &mut App, nth_latest: usize, width: usize) {
    let Some(record) = app.tool_log.iter().rev().nth(nth_latest.saturating_sub(1)) else {
        push_notice(app, "nothing to expand");
        return;
    };
    let (call_line, tool_name, output, inner) = (
        record.call_line.clone(),
        record.tool_name.clone(),
        record.output.clone(),
        record.inner.clone(),
    );
    expand_tool_record(
        app,
        &call_line,
        &tool_name,
        &output,
        &inner,
        width,
        EXPAND_MAX_LINES,
    );
}

/// The bounded, automatically shown preview for a finished user shell
/// (`!cmd`): the same `┌ │ └` expansion visuals as `/expand`, but capped
/// so a long output never floods the transcript unsolicited. `/expand 1`
/// afterwards still reveals the full result.
pub(crate) fn expand_last_shell(app: &mut App, width: usize) {
    let Some(record) = app
        .tool_log
        .iter()
        .rev()
        .find(|record| record.tool_name == "shell")
    else {
        return;
    };
    let (call_line, tool_name, output, inner) = (
        record.call_line.clone(),
        record.tool_name.clone(),
        record.output.clone(),
        record.inner.clone(),
    );
    expand_tool_record(
        app,
        &call_line,
        &tool_name,
        &output,
        &inner,
        width,
        SHELL_PREVIEW_MAX_LINES,
    );
}

fn expand_tool_record(
    app: &mut App,
    call_line: &str,
    tool_name: &str,
    output: &serde_json::Value,
    inner: &[String],
    width: usize,
    max_lines: usize,
) {
    let t = theme();
    let lines = view::expand_output(tool_name, output);
    let mut rendered = Vec::new();
    rendered.push(Line::from(vec![
        Span::styled("  ┌ ", t.dim),
        Span::styled(call_line.to_string(), t.accent),
    ]));
    let body_width = width.saturating_sub(6).max(16);
    for line in lines.iter().take(max_lines) {
        rendered.push(Line::from(vec![
            Span::styled("  │ ", t.dim),
            Span::raw(view::truncate_line(line, body_width)),
        ]));
    }
    if lines.len() > max_lines {
        rendered.push(Line::from(Span::styled(
            format!("  │ … {} more lines", lines.len() - max_lines),
            t.dim,
        )));
    }
    if !inner.is_empty() {
        rendered.push(Line::from(vec![
            Span::styled("  │ ", t.dim),
            Span::styled("inner activity:", t.dim),
        ]));
        for line in inner {
            rendered.push(Line::from(vec![
                Span::styled("  │   ", t.dim),
                Span::raw(view::truncate_line(line, body_width.saturating_sub(2))),
            ]));
        }
    }
    rendered.push(Line::from(Span::styled("  └", t.dim)));
    app.pending_history.extend(rendered);
}

/// Reveal the most recent completed turn's work tree. Raw output remains
/// available through `/expand n`, so this shortcut can focus on structure.
pub(crate) fn expand_latest_work(app: &mut App) -> bool {
    let Some(work) = app.work_log.last() else {
        return false;
    };
    if work.turn != app.turn_count {
        return true;
    }
    if work.expanded {
        return true;
    }

    let summaries = work.summaries.clone();
    let lines = work.lines.clone();
    let inserted = replace_block(&mut app.pending_history, &summaries, &lines)
        || replace_block(&mut app.transcript, &summaries, &lines);
    if inserted {
        if let Some(work) = app.work_log.last_mut() {
            work.expanded = true;
        }
    }
    true
}
