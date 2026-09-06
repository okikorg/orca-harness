use super::*;
use crate::view::{
    self,
    glyphs::{glyphs, set_ui_style, UiStyle},
    theme,
};
use orca_harness_tools::{AskOption, AskQuestion, AskRequest, AskTopic};
use ratatui::{backend::TestBackend, text::Line, widgets::Paragraph, Terminal};

fn form() -> ask::AskForm {
    let (respond, _) = tokio::sync::oneshot::channel();
    ask::AskForm::new(AskRequest {
        call_id: "preview".into(),
        respond,
        topics: vec![AskTopic {
            id: "scope".into(),
            title: "Scope".into(),
            questions: vec![AskQuestion {
                id: "direction".into(),
                question: "Which visual direction should be applied to the components?".into(),
                options: vec![AskOption {
                    label: "Subtle refinement with existing layout".into(),
                    description: Some(
                        "Keep the current layout and improve emphasis, alignment, and wrapping."
                            .into(),
                    ),
                }],
                multiple: false,
            }],
        }],
    })
}

fn transcript(width: usize, frame: usize) -> Vec<Line<'static>> {
    let t = theme();
    let g = glyphs();
    let mut lines = message::user_prompt(
        "Review the TUI components. Keep the refinements compact.",
        width,
        t.strong,
    );
    let mut work = activity_rail::ActivityRail::new(
        activity_rail::ActivityRailKind::Work,
        &g.active_frame(frame).to_string(),
        "2 tools",
        t.dim,
    );
    for (last, mark, call, detail, elapsed) in [
        (
            false,
            g.done.to_string(),
            "read_file components/picker.rs",
            "419 lines",
            "8ms",
        ),
        (
            true,
            g.running_frame(frame).to_string(),
            "shell cargo test",
            "running",
            "0.4s",
        ),
    ] {
        work.push(
            tool_row::ToolRow {
                branch: tree::TreeBranch {
                    indent: "    ",
                    last,
                },
                glyph: &mark,
                call,
                detail,
                elapsed,
                connector: tree::Connector::None,
                width,
                branch_style: t.dim,
                glyph_style: t.accent,
                call_style: t.strong,
            }
            .line(),
        );
    }
    work.append_to(&mut lines);
    lines.push(Line::default());
    lines.extend(message::assistant_message(
        "The main improvements are **clearer selection** and more consistent wrapping.",
        width,
    ));
    lines.extend(form().lines(width));
    lines.push(Line::default());
    lines.extend(
        composer::Composer::new(
            "Keep it compact",
            15,
            "Describe a task",
            width,
            t.accent,
            t.dim,
            &[],
        )
        .render()
        .lines,
    );
    let mut bar = status_bar::StatusBar::new();
    bar.push(status_bar::Segment::new("idle", status_bar::KEEP))
        .push(status_bar::Segment::new("ctx 18%", status_bar::CONTEXT))
        .push(status_bar::Segment::new("enter send", status_bar::HINT))
        .trailing(status_bar::Segment::new(
            "orca-harness",
            status_bar::WORKSPACE,
        ));
    lines.push(bar.line(width, t.dim));
    lines
}

fn screen(lines: Vec<Line<'static>>, width: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, lines.len() as u16)).unwrap();
    terminal
        .draw(|frame| frame.render_widget(Paragraph::new(lines), frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn component_widths_and_cursor_stay_inside_the_pane() {
    let t = theme();
    for width in [0, 1, 4, 12, 24, 40, 80, 120] {
        let mut lines = form().lines(width);
        lines.extend(
            approval::ApprovalPrompt {
                tool_name: "shell",
                detail: "cargo test -p orcacode components",
                yes_no: false,
            }
            .lines(width),
        );
        lines.extend(inspector::inspector_text(
            "日本語 and a long description for the inspector",
            width,
            t.dim,
        ));
        lines.extend(inspector::inspector_fields(
            [("日本語", "value".into()), ("status", "done".into())],
            width,
        ));
        lines.extend(
            inspector::CodePreview {
                text: "a long line of plain output",
                language: "text",
                width,
                indent: "    ",
                plain_style: t.dim,
            }
            .lines(),
        );
        for connector in [
            tree::Connector::None,
            tree::Connector::Reserved,
            tree::Connector::Drawn,
        ] {
            lines.push(
                tool_row::ToolRow {
                    branch: tree::TreeBranch {
                        indent: "    ",
                        last: true,
                    },
                    glyph: "✓",
                    call: "read_file 日本語.md",
                    detail: "read 120 bytes",
                    elapsed: "12ms",
                    connector,
                    width,
                    branch_style: t.dim,
                    glyph_style: t.success,
                    call_style: t.strong,
                }
                .line(),
            );
            lines.push(
                subagent_row::SubagentRow {
                    branch: tree::TreeBranch {
                        indent: "    ",
                        last: true,
                    },
                    glyph: "✓",
                    identity: "provider/model",
                    task: "Review components",
                    elapsed: "12s",
                    connector,
                    width,
                    branch_style: t.dim,
                    glyph_style: t.success,
                    label_style: t.strong,
                    identity_style: t.strong,
                    task_style: t.dim,
                }
                .line(),
            );
        }
        lines.extend(progress_list::progress_list(
            "A long progress label",
            &[progress_list::ProgressItem {
                content: "Review the rendering",
                state: progress_list::ProgressState::Active,
            }],
            width,
        ));
        lines.extend(message::user_prompt(
            "a long user prompt with 日本語\n\nmore text",
            width,
            t.strong,
        ));
        let welcome = welcome::Welcome {
            version: "0.1",
            model: "provider/model",
            workspace: "/workspace/orca-harness",
        }
        .lines(20, 5, width);
        assert!(welcome.len() <= 5);
        lines.extend(welcome);
        lines.extend(picker::ListPicker::new(1).windowed_lines(
            "A very long catalog header · enter select · esc close",
            ["row".into()],
            width,
            1,
        ));
        for text in ["", "long input with 日本語"] {
            let rendered = composer::Composer::new(
                text,
                text.chars().count(),
                "Describe a task to begin",
                width,
                t.accent,
                t.dim,
                &[],
            )
            .render();
            assert!(rendered.cursor_x as usize <= width.saturating_sub(1));
            lines.extend(rendered.lines);
        }
        assert!(
            lines.iter().all(|line| line.width() <= width),
            "width {width}: {lines:?}"
        );
    }
}

#[test]
fn spinner_changes_only_marker_cells_and_completion_is_static() {
    for style in UiStyle::ALL {
        set_ui_style(style);
        let a = screen(transcript(90, 0), 90);
        let b = screen(transcript(90, 1), 90);
        let differences: Vec<_> = a.chars().zip(b.chars()).filter(|(a, b)| a != b).collect();
        assert_eq!(
            differences.len(),
            1,
            "only the Work marker moves; running tools keep their square"
        );
        assert!(a.contains("✓ Read · "));
        assert!(a.contains("□ Run · cargo test"));
        assert!(b.contains("□ Run · cargo test"));
        for mark in glyphs().running {
            assert_eq!(view::cell_width(&mark.to_string()), 1);
        }
    }
    set_ui_style(UiStyle::Minimal);
}

#[test]
fn narrow_questions_and_approval_choices_keep_their_meaning() {
    let text = |lines: Vec<Line<'static>>| {
        lines
            .into_iter()
            .flat_map(|line| line.spans)
            .map(|s| s.content.into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    };
    for width in [40, 80, 120] {
        let lines = form().lines(width);
        for hint in [
            "↑↓ question",
            "←→ option",
            "space choose",
            "tab next topic",
            "enter send",
            "esc cancel",
        ] {
            assert!(
                lines.iter().any(|line| line.to_string().contains(hint)),
                "hint {hint} split at width {width}"
            );
        }
    }
    let question = text(form().lines(40));
    assert!(question.contains("components?"));
    assert!(question.contains("layout"));
    assert!(question.contains("esc cancel"));
    let approval = text(
        approval::ApprovalPrompt {
            tool_name: "shell",
            detail: "cargo test",
            yes_no: false,
        }
        .lines(20),
    );
    assert!(approval.contains("workspace"));
    assert!(approval.contains("session"));
}

#[test]
fn inspector_wide_labels_align_values_by_terminal_cells() {
    let lines = inspector::inspector_fields(
        [("日本語", "first".into()), ("status", "second".into())],
        40,
    );
    assert_eq!(lines[0].spans[0].width(), lines[1].spans[0].width());
}

#[test]
fn render_component_gallery() {
    for style in UiStyle::ALL {
        set_ui_style(style);
        for width in [40, 80, 120] {
            let output = screen(transcript(width, 0), width as u16);
            assert!(output.contains("enter send"));
            println!("\n--- {} / {width} columns ---\n{output}", style.label());
        }
    }
    set_ui_style(UiStyle::Minimal);
}

#[test]
fn render_readable_tool_failures() {
    let t = theme();
    for width in [60, 100] {
        let mut lines = vec![Line::from("  □ Work · 3 tools")];
        for (path, failed, last, elapsed) in [
            ("docs/architecture/overview.md", true, false, "496ms"),
            ("docs/design.html", false, false, "1ms"),
            ("docs/configuration/default.yaml", true, true, "500ms"),
        ] {
            let row = tool_row::ToolRow {
                branch: tree::TreeBranch {
                    indent: "    ",
                    last,
                },
                glyph: if failed { "×" } else { "✓" },
                call: &format!("read_file {path}"),
                detail: if failed { "" } else { "54.3 kB" },
                elapsed,
                connector: tree::Connector::None,
                width,
                branch_style: t.dim,
                glyph_style: if failed { t.error } else { t.success },
                call_style: t.accent,
            };
            lines.push(row.line());
            if failed {
                lines.extend(tool_error::lines(&serde_json::json!({"error": format!("read failed: No such file or directory (os error 2); nearest existing directory {} is empty", path.rsplit_once('/').unwrap().0)}), width, row.continuation()));
            }
        }
        let rendered = screen(lines, width as u16);
        assert!(rendered.contains("Read · docs/"));
        assert!(rendered.contains("File not found"));
        assert!(!rendered.contains("read_file"));
        assert!(!rendered.contains("\"error\""));
        println!("\n--- Tool rows / {width} columns ---\n{rendered}");
    }
}
