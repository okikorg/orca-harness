#[test]
fn agent_browser_reuses_terminal_body_and_invalidates_changed_projection() {
    let _theme = THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
    let original_theme = view::theme_name();
    let original_style = view::glyphs::ui_style();
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_test_subagent(&mut app, &worker);
    finish_test_subagent(
        &mut app,
        &worker,
        7,
        "outer-7",
        HarnessEvent::Result {
            message: "## Cached answer\n\nThe completed transcript survives redraws.".into(),
        },
    );
    app.agent_browser = Some(crate::tui::state::AgentBrowser::new(1));
    app.agent_browser.as_mut().unwrap().tab = crate::tui::state::AgentTab::All;
    let first_frame = rendered_rows(&mut app, 160, 36);
    let cached = |app: &App| {
        std::sync::Arc::clone(
            &app.agent_browser
                .as_ref()
                .unwrap()
                .body_cache
                .as_ref()
                .unwrap()
                .lines,
        )
    };
    let first = cached(&app);
    let next_frame = rendered_rows(&mut app, 160, 36);
    assert!(
        std::sync::Arc::ptr_eq(&first, &cached(&app)),
        "unchanged frame must reuse the rendered body allocation"
    );
    assert_eq!(first_frame, next_frame);
    assert!(first_frame.join("\n").contains("Cached answer"));

    rendered_rows(&mut app, 100, 36);
    let resized = cached(&app);
    assert!(!std::sync::Arc::ptr_eq(&first, &resized));
    view::set_theme(if original_theme == view::ThemeName::Mono {
        view::ThemeName::Default
    } else {
        view::ThemeName::Mono
    });
    rendered_rows(&mut app, 100, 36);
    let themed = cached(&app);
    assert!(!std::sync::Arc::ptr_eq(&resized, &themed));
    view::glyphs::set_ui_style(if original_style == view::glyphs::UiStyle::Minimal {
        view::glyphs::UiStyle::Glyph
    } else {
        view::glyphs::UiStyle::Minimal
    });
    rendered_rows(&mut app, 100, 36);
    let restyled = cached(&app);
    assert!(!std::sync::Arc::ptr_eq(&themed, &restyled));
    app.subagent_transcripts.get_mut(&7).unwrap().push_entry(
        crate::tui::state::SubagentTranscriptEntry::Assistant("late update".into()),
    );
    let updated = rendered_rows(&mut app, 100, 36);
    assert!(!std::sync::Arc::ptr_eq(&restyled, &cached(&app)));
    assert!(updated.join("\n").contains("late update"));

    app.subagent_transcripts.get_mut(&7).unwrap().status =
        crate::tui::state::SubagentTranscriptStatus::Running;
    rendered_rows(&mut app, 100, 36);
    assert!(
        app.agent_browser.as_ref().unwrap().body_cache.is_none(),
        "live activity must bypass the terminal cache"
    );
    view::set_theme(original_theme);
    view::glyphs::set_ui_style(original_style);
}
