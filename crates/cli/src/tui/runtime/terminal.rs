use super::super::*;

pub async fn run(
    cfg: TuiConfig,
    worker: mpsc::UnboundedSender<WorkerCmd>,
    mut ui_rx: mpsc::UnboundedReceiver<UiMsg>,
) -> io::Result<()> {
    enable_raw_mode()?;
    // Mouse capture is on for the whole session: without it the terminal
    // never forwards the wheel to a fullscreen app. Drag-select survives it
    // — terminals keep a modifier drag (option on macOS, shift elsewhere)
    // for their own selection while an app holds the mouse.
    // Bracketed paste arrives with it: without it a multi-line paste is
    // delivered as ordinary key events, so every newline reads as enter
    // and each pasted line is submitted or queued as its own prompt.
    crossterm::execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
        default_panic(info);
    }));

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(cfg);
    super::event_loop::run_loop(
        &mut terminal,
        &mut app,
        &worker,
        &mut ui_rx,
        CtEventStream::new(),
        crate::shutdown_signal(),
    )
    .await?;

    crossterm::execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    disable_raw_mode()?;
    println!(
        "orcacode · session ended · tokens in {} out {}",
        app.tokens_in, app.tokens_out
    );
    Ok(())
}

/// Move the transcript view by `delta` lines: positive scrolls back
/// through history, negative returns toward the newest line. The upper
/// bound belongs to the renderer, which is the only place that knows how
/// many wrapped rows the transcript occupies at the current width.
pub(crate) fn scroll_transcript(app: &mut App, delta: isize) {
    if delta >= 0 {
        app.scroll = app.scroll.saturating_add(delta as usize);
    } else {
        app.scroll = app.scroll.saturating_sub(delta.unsigned_abs());
    }
    // Scrolling back is the moment someone is looking for something to
    // copy, so it is the moment worth spending the status line on. The
    // hint expires; a permanent one would just be furniture.
    app.scroll_hint_at = Some(Instant::now());
}

/// `/copy [code|all]` — push text to the terminal's clipboard.
///
/// Native selection only ever reaches the visible viewport, so the parts
/// most worth copying — a long answer, a code block that scrolled past —
/// need a path that does not go through the mouse at all.
pub(crate) fn copy_command(app: &mut App, arg: &str) {
    // Mid-stream, the newest text is still in the delta buffer and has
    // not become an answer yet. Copying the previous turn's answer under
    // a notice that says "last answer" would be a quiet wrong result.
    let streaming = (!app.text.trim().is_empty()).then(|| app.text.clone());
    let (label, text) = match arg {
        "" | "last" | "answer" => match streaming {
            Some(partial) => ("answer so far", Some(partial)),
            None => ("last answer", app.last_answer.clone()),
        },
        "code" | "block" => match streaming.as_deref().and_then(clipboard::last_code_block) {
            Some(block) => ("code block so far", Some(block)),
            None => (
                "last code block",
                app.last_answer
                    .as_deref()
                    .and_then(clipboard::last_code_block),
            ),
        },
        "all" | "transcript" => ("transcript", Some(transcript_text(app))),
        "tool" | "pane" => ("inspected tool", inspected_tool_text(app)),
        other => {
            push_error(
                app,
                format!("unknown /copy target: {other} — /copy [code|all|tool]"),
            );
            return;
        }
    };
    let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
        push_error(app, format!("nothing to copy: no {label} yet"));
        return;
    };
    let bytes = text.len();
    if bytes > clipboard::MAX_COPY_BYTES {
        push_error(
            app,
            format!(
                "{label} is {}KB — past the {}KB a terminal will accept in one clipboard write",
                bytes / 1024,
                clipboard::MAX_COPY_BYTES / 1024,
            ),
        );
        return;
    }
    let lines = text.lines().count();
    let plural = if lines == 1 { "" } else { "s" };
    app.clipboard_pending = Some(text);
    push_notice(
        app,
        format!("copied {label} ({lines} line{plural}) — under tmux this needs set-clipboard on"),
    );
}

/// The inspected tool as plain text: its call line, its input, and its
/// output. In split view a native drag cannot stay inside one pane — the
/// terminal selects whole rows across both — so this is how the right
/// pane comes out on its own.
pub(crate) fn inspected_tool_text(app: &App) -> Option<String> {
    let tool = split_inspected_tool(app)?;
    let mut out = String::new();
    out.push_str(tool.call_line.trim());
    out.push('\n');
    if let Ok(input) = serde_json::to_string_pretty(&tool.input) {
        out.push_str("\ninput\n");
        out.push_str(&input);
        out.push('\n');
    }
    match &tool.output {
        Some(output) => {
            out.push_str("\noutput\n");
            out.push_str(inspector_text_content(output).trim_end());
            out.push('\n');
        }
        None => out.push_str("\noutput\nwaiting for result\n"),
    }
    Some(out)
}

/// The tool the inspector is showing: the selected one, else the latest.
pub(crate) fn split_inspected_tool(app: &App) -> Option<&ToolActivity> {
    let selected = app
        .split_tool
        .unwrap_or_else(|| app.activity_tools.len().saturating_sub(1));
    app.activity_tools
        .get(selected)
        .or_else(|| app.activity_tools.last())
        .or(app.split_snapshot.as_ref())
}

/// The transcript rendered back to plain text, styling and layout
/// dropped. Trailing blank lines from block spacing go with it; a
/// clipboard payload that ends in six empty rows is nobody's intent.
pub(crate) fn transcript_text(app: &App) -> String {
    let mut lines: Vec<String> = app
        .transcript
        .iter()
        .chain(app.pending_history.iter())
        .map(|line| line_text(line).trim_end().to_string())
        .collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// A compact transcript notification: the dot keeps system status scannable
/// without giving it the visual weight of transcript content.
pub(crate) fn push_notice(app: &mut App, text: impl Into<String>) {
    app.push_line(Notification::notice(text).line());
}

/// [`push_notice`] — a failed command is still the system talking, and a
/// line without the glyph reads as model output — with the body in the
/// error color so severity and origin are two separate cues.
pub(crate) fn push_error(app: &mut App, text: impl Into<String>) {
    app.push_line(Notification::error(text).line());
}
