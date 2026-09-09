#[tokio::test]
async fn streaming_event_bursts_preserve_order_and_share_frames() {
    let mut app = streaming_app();
    app.text.clear();
    let (worker, _commands) = mpsc::unbounded_channel();
    let (ui, mut messages) = mpsc::unbounded_channel();
    let expected: String = (0..1024).map(|i| format!("{i},")).collect();
    for i in 0..1024 {
        ui.send(UiMsg::Event(HarnessEvent::AssistantDelta {
            text: format!("{i},"),
        }))
        .unwrap();
    }
    drop(ui);
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
    super::runtime::event_loop::run_loop(
        &mut terminal,
        &mut app,
        &worker,
        &mut messages,
        futures_util::stream::pending(),
        std::future::pending(),
    )
    .await
    .unwrap();

    assert_eq!(app.text, expected, "every delta must be applied in order");
    assert!(
        terminal.get_frame().count() < 32,
        "a burst must not draw once per event"
    );
    let screen = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        screen.contains("1023,"),
        "channel close must flush the final frame"
    );
}

#[tokio::test]
async fn keyboard_cancellation_preempts_a_worker_backlog() {
    let mut app = streaming_app();
    let cancel = match &app.run {
        RunState::Running { cancel, .. } => cancel.clone(),
        RunState::Idle => unreachable!(),
    };
    let (worker, _commands) = mpsc::unbounded_channel();
    let (ui, mut messages) = mpsc::unbounded_channel();
    for _ in 0..4096 {
        ui.send(UiMsg::Event(HarnessEvent::AssistantDelta {
            text: "x".into(),
        }))
        .unwrap();
    }
    let mut polls = 0;
    let input = futures_util::stream::poll_fn(move |_| {
        polls += 1;
        match polls {
            1 => std::task::Poll::Pending,
            2 => std::task::Poll::Ready(Some(Ok(CtEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            ))))),
            _ => std::task::Poll::Ready(None),
        }
    });
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
    super::runtime::event_loop::run_loop(
        &mut terminal,
        &mut app,
        &worker,
        &mut messages,
        input,
        std::future::pending(),
    )
    .await
    .unwrap();

    assert!(cancel.is_cancelled());
    assert!(
        messages.len() >= 4096 - 64,
        "input must be polled between bounded batches"
    );
}

#[tokio::test]
async fn keyboard_edits_draw_immediately_and_quit_skips_the_final_draw() {
    for (key, composer, frames) in [
        (
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            "q",
            2,
        ),
        (
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            "",
            1,
        ),
    ] {
        let mut app = streaming_app();
        let (worker, _commands) = mpsc::unbounded_channel();
        let (_ui, mut messages) = mpsc::unbounded_channel();
        let input = futures_util::stream::iter([Ok(CtEvent::Key(key))]);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
        super::runtime::event_loop::run_loop(
            &mut terminal,
            &mut app,
            &worker,
            &mut messages,
            input,
            std::future::pending(),
        )
        .await
        .unwrap();

        assert_eq!(app.composer, composer);
        assert_eq!(
            terminal.get_frame().count(),
            frames,
            "edits must draw before the next input poll; quit must exit without drawing"
        );
    }
}

#[tokio::test]
async fn streaming_frame_deadline_flushes_without_another_event() {
    let mut app = streaming_app();
    app.text.clear();
    let (worker, _commands) = mpsc::unbounded_channel();
    let (ui, mut messages) = mpsc::unbounded_channel();
    ui.send(UiMsg::Event(HarnessEvent::AssistantDelta {
        text: "fresh output".into(),
    }))
    .unwrap();
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
    super::runtime::event_loop::run_loop(
        &mut terminal,
        &mut app,
        &worker,
        &mut messages,
        futures_util::stream::pending(),
        tokio::time::sleep(Duration::from_millis(60)),
    )
    .await
    .unwrap();

    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        screen.contains("fresh output"),
        "a lone update must reach its scheduled frame while the channel stays open"
    );
    assert_eq!(
        terminal.get_frame().count(),
        2,
        "unchanged frames should not redraw between animation ticks"
    );
}

#[tokio::test]
async fn approval_is_drawn_before_following_worker_events() {
    let mut app = test_app();
    let (worker, _commands) = mpsc::unbounded_channel();
    let (ui, mut messages) = mpsc::unbounded_channel();
    let (respond, _answer) = tokio::sync::oneshot::channel();
    ui.send(UiMsg::Approval(crate::msg::ApprovalRequest {
        tool_name: "shell".into(),
        detail: "shell $ cargo test".into(),
        yes_no: false,
        respond,
    }))
    .unwrap();
    ui.send(UiMsg::Event(HarnessEvent::AssistantDelta {
        text: "later".into(),
    }))
    .unwrap();
    let mut polls = 0;
    let input = futures_util::stream::poll_fn(move |_| {
        polls += 1;
        if polls == 1 {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(None)
        }
    });
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
    super::runtime::event_loop::run_loop(
        &mut terminal,
        &mut app,
        &worker,
        &mut messages,
        input,
        std::future::pending(),
    )
    .await
    .unwrap();

    assert_eq!(messages.len(), 1, "approval must stop the current batch");
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        screen.contains("approval required"),
        "approval must render without waiting for the frame deadline"
    );
}

#[test]
fn cached_streaming_frames_preserve_cells_and_do_not_accumulate_carets() {
    let _guard = super::THEME_GUARD.lock().unwrap();
    let mut app = streaming_app();
    set_ui_style(UiStyle::Glyph);
    // Keep the live region static; its elapsed clock is intentionally not
    // cached and would otherwise change between these cell comparisons.
    let (respond, _answer) = tokio::sync::oneshot::channel();
    app.approval = Some(crate::msg::ApprovalRequest {
        tool_name: "shell".into(),
        detail: "shell $ cargo test".into(),
        yes_no: false,
        respond,
    });
    app.text = "# Example\n```rust\nlet value = 42;\n```\nDone.".into();
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let first = terminal.backend().buffer().clone();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert_eq!(&first, terminal.backend().buffer());
    app.streaming_markdown.borrow_mut().clear();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert_eq!(
        &first,
        terminal.backend().buffer(),
        "cached and fresh draws must have identical styled cells"
    );
    let screen: String = first.content.iter().map(|cell| cell.symbol()).collect();
    assert_eq!(screen.matches('▏').count(), 1);
    set_ui_style(UiStyle::Minimal);
}
