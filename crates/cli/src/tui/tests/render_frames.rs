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
fn mermaid_flowchart_reaches_the_rendered_transcript_buffer() {
    let mut app = test_app();
    app.welcome_dismissed = true;
    app.transcript.clear();
    app.pending_history.clear();
    app.push_markdown_block(
        "```mermaid\nflowchart TD\nCLI[host] --> CORE[kernel]\n```",
        80,
        BlockSpacing::Section,
    );
    app.absorb_pending();

    let screen = rendered_rows(&mut app, 80, 16).join("\n");
    assert!(screen.contains("CLI · host"), "{screen}");
    assert!(screen.contains("CORE · kernel"), "{screen}");
    assert!(screen.contains("┌"), "{screen}");
    assert!(screen.contains("▼"), "{screen}");
    assert!(!screen.contains("flowchart TD"), "{screen}");
}

#[test]
fn nested_system_flowchart_renders_in_the_terminal_buffer() {
    let source = r#"```mermaid
flowchart TB
    %% Boundary
    subgraph Outside["Surrounding system"]
        CP["Control plane\nscheduling · sessions · networking · tenancy · fleet"]
    end

    subgraph Workspace["orca-harness Rust workspace"]
        direction TB

        subgraph Host["Host layer"]
            CLI["crates/cli\norcacode terminal host"]
            SDK["crates/sdk\nembedding API"]
        end

        subgraph Kernel["Execution kernel"]
            CORE["crates/harness-core\nagent loop · dispatcher · limits · extensions hooks"]
            EXT["crates/extensions\npolicy · memory · compaction · retry · sessions"]
            TOOLS["crates/tools\nshell · fs · process · python/bun · subagents · todo"]
            TOOLX["crates/tool-extensions\nMCP · skills · web"]
            MP["crates/model-providers\nOpenAI · OpenRouter · Codex adapters"]
            AUTH["crates/provider-auth\ncredential contracts"]
        end
    end

    CP --> CLI
    CLI --> CORE
    CLI --> MP
    CLI --> AUTH
    CLI --> TOOLS
    CLI --> TOOLX
    CLI --> EXT
    CLI --> SDK
    SDK --> CORE
    SDK --> MP
    SDK --> AUTH
    SDK --> TOOLS
    SDK --> TOOLX
    SDK --> EXT
    CORE --> MP
    CORE --> TOOLS
    CORE --> TOOLX
    CORE --> EXT
    MP --> AUTH
    CORE -->|"model responses"| CORE
    CORE -->|"tool calls"| TOOLS
    CORE -->|"optional integrations"| TOOLX
    CORE -->|"policy / lifecycle hooks"| EXT

    style CORE fill:#1f2937,color:#fff,stroke:#60a5fa,stroke-width:2px
    style CLI fill:#111827,color:#fff,stroke:#34d399,stroke-width:2px
    style SDK fill:#111827,color:#fff,stroke:#34d399,stroke-width:1.5px
```"#;
    let mut app = test_app();
    app.welcome_dismissed = true;
    app.transcript.clear();
    app.pending_history.clear();
    app.push_markdown_block(source, 180, BlockSpacing::Section);
    app.absorb_pending();

    let screen = rendered_rows(&mut app, 180, 52).join("\n");
    assert!(screen.contains("CP · Control plane"), "{screen}");
    assert!(screen.contains("CLI · crates/cli"), "{screen}");
    assert!(screen.contains("CORE · crates/harness"), "{screen}");
    assert!(screen.contains('▲'), "{screen}");
    assert!(!screen.contains("flowchart TB"), "{screen}");
    assert!(!screen.contains("direction TB"), "{screen}");
}

#[test]
fn streaming_mermaid_holds_a_stable_placeholder_until_the_fence_closes() {
    let mut app = test_app();
    app.welcome_dismissed = true;
    app.transcript.clear();
    app.pending_history.clear();
    app.run = RunState::Running {
        started: Instant::now(),
        cancel: CancellationToken::new(),
    };

    handle_harness_event(
        &mut app,
        HarnessEvent::AssistantDelta {
            text: "```mermaid\nflowchart TD\nH[Host]".into(),
        },
        80,
    );
    let first = rendered_rows(&mut app, 80, 16).join("\n");
    assert_eq!(first.matches("waiting for closing fence").count(), 1, "{first}");
    assert!(!first.contains("H · Host"), "{first}");

    handle_harness_event(
        &mut app,
        HarnessEvent::AssistantDelta {
            text: "\nH --> A[Agent]".into(),
        },
        80,
    );
    let later = rendered_rows(&mut app, 80, 16).join("\n");
    assert_eq!(later.matches("waiting for closing fence").count(), 1, "{later}");
    assert!(!later.contains("H · Host"), "{later}");

    handle_harness_event(
        &mut app,
        HarnessEvent::AssistantDelta {
            text: "\n```".into(),
        },
        80,
    );
    let complete = rendered_rows(&mut app, 80, 16).join("\n");
    assert!(!complete.contains("waiting for closing fence"), "{complete}");
    assert!(complete.contains("H · Host"), "{complete}");
    assert!(complete.contains("A · Agent"), "{complete}");
    assert!(complete.contains('▼'), "{complete}");
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
fn narrow_status_bar_keeps_the_hint() {
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
    assert!(last_row(&rows).contains("enter send"), "{rows:?}");
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

#[test]
fn a_tight_status_row_shortens_the_hint_before_dropping_the_model() {
    let mut app = test_app();
    let rows = rendered_rows(&mut app, 70, 20);
    let status = last_row(&rows);
    assert!(status.starts_with(" local:test ·"), "{status}");
    assert!(!status.contains("wheel scroll"), "{status}");
    assert!(status.contains("ctrl+o expand"), "{status}");
}

/// The model name alone reads the same on every provider that serves it;
/// the provider leads it, spelled out in both styles.
#[test]
fn status_row_leads_the_model_with_its_provider() {
    let mut app = test_app();
    with_style(UiStyle::Glyph, &mut app, |app| {
        let rows = rendered_rows(app, 120, 20);
        let status = last_row(&rows);
        assert!(status.starts_with(" local:test ·"), "{status}");
    });
}

#[test]
fn a_split_gives_the_transcript_pane_a_header() {
    let mut app = test_app();
    app.view_mode = ViewMode::Split;
    let rows = rendered_rows(&mut app, 120, 30);
    assert!(rows[0].starts_with("  transcript · turn 0"), "{:?}", rows[0]);
}

/// Run `body` with `style` active, restoring Minimal after. The store is
/// thread-local under test, so this cannot leak into another test.
fn with_style(style: UiStyle, app: &mut App, body: impl FnOnce(&mut App)) {
    set_ui_style(style);
    body(app);
    set_ui_style(UiStyle::Minimal);
}

fn streaming_app() -> App {
    let mut app = test_app();
    app.welcome_dismissed = true;
    app.run = RunState::Running {
        started: Instant::now(),
        cancel: CancellationToken::new(),
    };
    handle_harness_event(
        &mut app,
        HarnessEvent::AssistantDelta {
            text: "Streaming words".into(),
        },
        80,
    );
    app.context_tokens = 60_000;
    app.context_window = Some(200_000);
    app
}

#[test]
fn glyph_style_marks_the_stream_and_meters_the_context() {
    let mut app = streaming_app();
    with_style(UiStyle::Minimal, &mut app, |app| {
        let minimal = rendered_rows(app, 140, 20);
        assert!(
            minimal.iter().any(|row| row.ends_with("Streaming words")),
            "{minimal:?}"
        );
        let status = last_row(&minimal);
        assert!(status.contains("ctx 30%"), "{status}");
    });

    with_style(UiStyle::Glyph, &mut app, |app| {
        let glyph = rendered_rows(app, 140, 20);
        assert!(glyph.iter().any(|row| row.ends_with("Streaming words▏")), "{glyph:?}");
        let status = last_row(&glyph);
        assert!(status.contains("ctx ━━──── 30%"), "{status}");
        // The meter is decoration: it goes before any segment does.
        let tight = rendered_rows(app, 64, 20);
        let status = last_row(&tight);
        assert!(status.contains("ctx 30%") && !status.contains('━'), "{status}");
    });
}

#[test]
fn glyph_style_reads_every_mark_from_the_table() {
    let mut app = streaming_app();
    handle_harness_event(
        &mut app,
        HarnessEvent::ToolCall {
            tool_call_id: "c1".into(),
            tool_name: "shell".into(),
            input: serde_json::json!({"command": "true"}),
        },
        80,
    );
    with_style(UiStyle::Glyph, &mut app, |app| {
        let screen = rendered_rows(app, 100, 20).join("\n");
        let g = UiStyle::Glyph.glyphs();
        assert!(screen.contains(&format!("{} Work ·", g.active[0])), "{screen}");
        assert!(
            screen.contains(&format!("└─ {} shell $ true", g.running[0])),
            "{screen}"
        );
        assert_eq!(g.running, &['⬚']);
        assert_eq!(g.active, &['□', '■']);
        assert_eq!(g.section, '□');
        assert_eq!(g.attention, '!');
        assert_eq!(g.cursor, "›");
        assert_eq!(g.done, UiStyle::Minimal.glyphs().done);
        assert!(screen.contains("writing"), "{screen}");
    });
}
