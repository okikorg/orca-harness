// Whole-frame checks at fixed sizes. Every finding that depends on the
// terminal width lives here: the composer's rows, the status bar's
// priority order, the live region's anchoring, the approval legend and
// the stacked split all show up in a rendered buffer, not in a joined
// string of spans.

fn last_row(rows: &[String]) -> &str {
    rows.iter()
        .rev()
        .find(|row| !row.is_empty())
        .map(String::as_str)
        .expect("status row")
}

#[test]
fn approval_legend_reflows_on_a_sixty_column_frame() {
    let (respond, _answer) = tokio::sync::oneshot::channel();
    let mut app = test_app();
    app.approval = Some(crate::msg::ApprovalRequest {
        tool_name: "shell".into(),
        detail: "shell $ cargo test -p orcacode tui::components -- --nocapture".into(),
        yes_no: false,
        respond,
    });

    let rows = rendered_rows(&mut app, 60, 24);
    let screen = rows.join("\n");
    assert!(screen.contains("? approval required · shell"), "{screen}");
    assert!(screen.contains("│ shell $ cargo test"), "{screen}");
    let legend: Vec<&String> = rows
        .iter()
        .filter(|row| {
            row.starts_with("    ") && (row.contains("allow once") || row.contains("deny"))
        })
        .collect();
    assert_eq!(legend.len(), 2, "four choices over two rows: {screen}");
    assert!(
        rows.iter().all(|row| !row.starts_with("e)")),
        "no mid-word wrap: {screen}"
    );
    assert!(screen.contains("answering approval above"), "{screen}");
    let status = last_row(&rows);
    assert!(status.contains("awaiting approval"), "{status}");
    assert!(status.contains("y allow once"), "{status}");
}

#[test]
fn narrow_status_bar_keeps_the_run_state_and_the_hint() {
    let mut app = test_app();
    app.reasoning_effort = Some("high".into());
    app.prompt_queue.push_back("later".into());

    let rows = rendered_rows(&mut app, 64, 20);
    let status = last_row(&rows);
    assert!(status.contains("queue paused"), "{status}");
    assert!(status.contains("enter resume"), "{status}");
    assert!(!status.contains("effort"), "effort is the first to go: {status}");
    assert!(!status.ends_with('…'), "dropped, not cut: {status}");
}

#[test]
fn a_running_live_region_keeps_the_spinner_row_when_it_overflows() {
    let mut app = test_app();
    app.run = RunState::Running {
        started: Instant::now(),
        cancel: CancellationToken::new(),
    };
    for n in 0..6 {
        app.prompt_queue.push_back(format!("queued prompt {n}"));
    }

    // Ten rows: three for the transcript, the gap, composer and status,
    // which leaves four for a live region that wants six.
    let rows = rendered_rows(&mut app, 80, 10);
    let screen = rows.join("\n");
    assert!(screen.contains("esc to interrupt"), "{screen}");
    assert!(!screen.contains("queued · 6"), "the head goes first: {screen}");
    assert!(screen.contains("+3 more"), "{screen}");
}

#[test]
fn long_input_grows_the_composer_and_keeps_the_status_row_last() {
    let mut app = test_app();
    app.composer = "word ".repeat(40).trim_end().to_string();
    app.cursor = app.composer.chars().count();

    let rows = rendered_rows(&mut app, 60, 24);
    let composer_rows = rows.iter().filter(|row| row.starts_with("│ ")).count();
    assert_eq!(composer_rows, 4, "199 chars at 57 per row: {rows:?}");
    assert!(last_row(&rows).contains("idle"), "{rows:?}");
    assert_eq!(rows.iter().rposition(|row| !row.is_empty()), Some(23));
}

#[test]
fn a_narrow_split_stacks_the_inspector_under_the_transcript() {
    let mut app = test_app();
    app.view_mode = ViewMode::Split;

    let rows = rendered_rows(&mut app, 90, 30);
    let border = rows
        .iter()
        .position(|row| row.starts_with("───"))
        .expect("top border of the stacked inspector");
    assert!(border > 10 && border < 25, "inspector takes the lower part: {border}");
    let area = app.inspector_area.expect("inspector area recorded");
    assert_eq!(area.width, 90);
    assert_eq!(area.y as usize, border);

    // The same app on a wide terminal goes back to side by side.
    rendered_rows(&mut app, 120, 30);
    let area = app.inspector_area.expect("inspector area recorded");
    assert_eq!(area.y, 0);
    assert!(area.x >= 69 && area.x + area.width == 120, "{area:?}");
}
