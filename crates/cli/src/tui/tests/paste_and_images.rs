use super::input::insert_image_bytes_for_test;
use super::state::HeldInput;
use super::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use orca_harness_model_providers::openrouter::ModelInfo;
use ratatui::backend::TestBackend;
use ratatui::style::Style;

fn paste(app: &mut App, tx: &mpsc::UnboundedSender<WorkerCmd>, text: &str) {
    handle_terminal_event(
        app,
        CtEvent::Paste(text.to_string()),
        tx,
        TEST_TERMINAL_WIDTH,
    );
}

/// The bug this replaced: without bracketed paste every newline
/// arrived as enter, so a pasted block submitted its first line and
/// queued the rest.
#[test]
fn a_multiline_paste_is_one_marker_and_submits_nothing() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    let block = (1..=23)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");

    paste(&mut app, &tx, &block);

    assert_eq!(app.composer, "[Pasted text #1, 23 lines]");
    assert_eq!(app.cursor, app.composer.chars().count());
    assert!(app.prompt_queue.is_empty(), "paste must not queue prompts");
    assert!(rx.try_recv().is_err(), "paste must not start a run");
}

#[test]
fn a_marker_expands_to_the_held_text_on_send() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = test_app();

    paste(&mut app, &tx, "alpha\nbeta\ngamma");
    for c in " explain this".chars() {
        press(&mut app, &tx, KeyCode::Char(c));
    }
    assert_eq!(app.composer, "[Pasted text #1, 3 lines] explain this");
    submit(&mut app, &tx, TEST_TERMINAL_WIDTH);

    match rx.try_recv() {
        Ok(WorkerCmd::Run { prompt, .. }) => {
            assert_eq!(prompt, "alpha\nbeta\ngamma explain this");
        }
        other => panic!("expected a run, got {:?}", other.is_ok()),
    }
    // Once sent, the turn shows what the model got, not the marker.
    let shown = app
        .pending_history
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        shown.contains("alpha") && shown.contains("beta") && shown.contains("gamma"),
        "transcript should unfurl the paste: {shown}"
    );
    assert!(
        !shown.contains("[Pasted text #"),
        "no marker should survive into the transcript: {shown}"
    );
}

#[test]
fn backspace_removes_a_marker_whole() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    for c in "look at ".chars() {
        press(&mut app, &tx, KeyCode::Char(c));
    }
    paste(&mut app, &tx, "alpha\nbeta\ngamma");

    press(&mut app, &tx, KeyCode::Backspace);

    assert_eq!(app.composer, "look at ");
    assert_eq!(app.cursor, 8);
}

#[test]
fn delete_removes_a_marker_whole_from_its_start() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    paste(&mut app, &tx, "alpha\nbeta");
    for c in " done".chars() {
        press(&mut app, &tx, KeyCode::Char(c));
    }
    app.cursor = 0;

    press(&mut app, &tx, KeyCode::Delete);

    assert_eq!(app.composer, " done");
    assert_eq!(app.cursor, 0);
}

#[test]
fn arrows_step_over_a_marker_rather_than_into_it() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    paste(&mut app, &tx, "alpha\nbeta");
    let width = app.composer.chars().count();

    press(&mut app, &tx, KeyCode::Left);
    assert_eq!(app.cursor, 0, "left clears the whole marker");

    press(&mut app, &tx, KeyCode::Right);
    assert_eq!(app.cursor, width, "right clears the whole marker");
}

/// Backspacing the chip must not strand the next paste's numbering.
#[test]
fn a_removed_marker_leaves_later_markers_expanding() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    paste(&mut app, &tx, "first\nblock");
    press(&mut app, &tx, KeyCode::Backspace);
    assert_eq!(app.composer, "");

    paste(&mut app, &tx, "second\nblock");
    assert_eq!(app.composer, "[Pasted text #2, 2 lines]");
    submit(&mut app, &tx, TEST_TERMINAL_WIDTH);

    match rx.try_recv() {
        Ok(WorkerCmd::Run { prompt, .. }) => assert_eq!(prompt, "second\nblock"),
        other => panic!("expected a run, got {:?}", other.is_ok()),
    }
}

fn add_test_image(app: &mut App) {
    insert_image_bytes_for_test(app, b"\x89PNG\r\n\x1a\nminimal".to_vec());
}

#[test]
fn clipboard_image_is_an_atomic_pill_and_sends_native_data() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    app.cfg.provider = Provider::OpenAi;
    add_test_image(&mut app);

    assert_eq!(app.composer, "[▧ image.png]");
    let pill_end = app.cursor;
    press(&mut app, &tx, KeyCode::Left);
    assert_eq!(app.cursor, 0);
    press(&mut app, &tx, KeyCode::Right);
    assert_eq!(app.cursor, pill_end);

    submit(&mut app, &tx, TEST_TERMINAL_WIDTH);
    match rx.try_recv() {
        Ok(WorkerCmd::Run { prompt, images, .. }) => {
            assert_eq!(prompt, "[▧ image.png]");
            assert_eq!(images.len(), 1);
            assert_eq!(images[0].media_type, "image/png");
        }
        other => panic!("expected a run, got {:?}", other.is_ok()),
    }
}

#[test]
fn backspace_removes_an_image_pill_whole() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    add_test_image(&mut app);

    press(&mut app, &tx, KeyCode::Backspace);
    assert_eq!(app.composer, "");
    assert_eq!(app.cursor, 0);
}

#[test]
fn delete_removes_an_image_pill_whole_from_its_start() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    add_test_image(&mut app);
    app.cursor = 0;

    press(&mut app, &tx, KeyCode::Delete);
    assert_eq!(app.composer, "");
    assert_eq!(app.cursor, 0);
}

#[test]
fn pasted_image_path_remains_plain_text() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();

    paste(&mut app, &tx, "/tmp/screenshot.png");
    assert_eq!(app.composer, "/tmp/screenshot.png");
    assert!(app.pastes.is_empty());
}

#[test]
fn local_model_keeps_image_draft_when_send_is_blocked() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    add_test_image(&mut app);
    let draft = app.composer.clone();

    submit(&mut app, &tx, TEST_TERMINAL_WIDTH);
    assert_eq!(app.composer, draft);
    assert!(rx.try_recv().is_err());
}

#[test]
fn repeated_clipboard_images_get_distinct_pills() {
    let mut app = test_app();
    add_test_image(&mut app);
    add_test_image(&mut app);
    assert_eq!(app.composer, "[▧ image.png][▧ image.png · 2]");
}

#[test]
fn image_payloads_follow_visual_pill_order() {
    let first = HeldInput::Image {
        label: "first.png".into(),
        image: orca_harness_core::Image {
            media_type: "image/png".into(),
            data: "first".into(),
        },
    };
    let second = HeldInput::Image {
        label: "second.png".into(),
        image: orca_harness_core::Image {
            media_type: "image/png".into(),
            data: "second".into(),
        },
    };
    let held = vec![first, second];
    let prompt = format!("{} then {}", held[1].marker(2), held[0].marker(1));

    let images = prompt_images(&held, &prompt);
    assert_eq!(images[0].data, "second");
    assert_eq!(images[1].data, "first");
}

#[test]
fn a_short_single_line_paste_is_typed_through() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    for c in "run ".chars() {
        press(&mut app, &tx, KeyCode::Char(c));
    }

    paste(&mut app, &tx, "cargo test --all");

    assert_eq!(app.composer, "run cargo test --all");
    assert!(app.pastes.is_empty(), "nothing to hold aside");
}

#[test]
fn crlf_pastes_are_normalized_before_counting() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();

    paste(&mut app, &tx, "one\r\ntwo\r\nthree\r\n");

    assert_eq!(app.composer, "[Pasted text #1, 3 lines]");
    assert!(matches!(&app.pastes[0], HeldInput::Text(text) if text == "one\ntwo\nthree\n"));
}

#[test]
fn markers_number_upward_and_expand_independently() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = test_app();

    paste(&mut app, &tx, "first\nblock");
    paste(&mut app, &tx, "second\nblock");
    assert_eq!(
        app.composer,
        "[Pasted text #1, 2 lines][Pasted text #2, 2 lines]"
    );

    submit(&mut app, &tx, TEST_TERMINAL_WIDTH);
    match rx.try_recv() {
        Ok(WorkerCmd::Run { prompt, .. }) => {
            assert_eq!(prompt, "first\nblocksecond\nblock");
        }
        other => panic!("expected a run, got {:?}", other.is_ok()),
    }
}

/// Text that merely looks like a marker is not a marker.
#[test]
fn typed_marker_lookalikes_are_left_alone() {
    assert_eq!(
        expand_pastes(&[], "[Pasted text #1, 3 lines]"),
        "[Pasted text #1, 3 lines]"
    );
    assert_eq!(
        expand_pastes(
            &[HeldInput::Text("a\nb".to_string())],
            "[Pasted text #1, 9 lines]",
        ),
        "[Pasted text #1, 9 lines]"
    );
}
