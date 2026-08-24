use super::super::*;

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
    if worker
        .send(WorkerCmd::Shell {
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
    let images = prompt_images(&app.pastes, &prompt);
    // Text paste markers are composer affordances; image pills remain in
    // the visible/model text as references alongside their native payloads.
    let prompt = expand_pastes(&app.pastes, &prompt);
    app.context_tokens += (prompt.len() / 4) as u64;
    if worker
        .send(WorkerCmd::Run {
            prompt: strip_location_mentions(&prompt),
            images,
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
        started: Instant::now(),
        cancel,
    };
    true
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
    let t = theme();
    let Some(record) = app.tool_log.iter().rev().nth(nth_latest.saturating_sub(1)) else {
        push_notice(app, "nothing to expand");
        return;
    };
    let lines = view::expand_output(&record.tool_name, &record.output);
    let mut rendered = Vec::new();
    rendered.push(Line::from(vec![
        Span::styled("  ┌ ", t.dim),
        Span::styled(record.call_line.clone(), t.accent),
    ]));
    let body_width = width.saturating_sub(6).max(16);
    for line in lines.iter().take(EXPAND_MAX_LINES) {
        rendered.push(Line::from(vec![
            Span::styled("  │ ", t.dim),
            Span::raw(view::truncate_line(line, body_width)),
        ]));
    }
    if lines.len() > EXPAND_MAX_LINES {
        rendered.push(Line::from(Span::styled(
            format!("  │ … {} more lines", lines.len() - EXPAND_MAX_LINES),
            t.dim,
        )));
    }
    if !record.inner.is_empty() {
        rendered.push(Line::from(vec![
            Span::styled("  │ ", t.dim),
            Span::styled("inner activity:", t.dim),
        ]));
        for line in &record.inner {
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
