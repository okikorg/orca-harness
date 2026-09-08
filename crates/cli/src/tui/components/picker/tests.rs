use super::*;

#[test]
fn navigation_clamps_to_the_list() {
    let mut picker = ListPicker::new(3);
    assert!(matches!(picker.on_key(KeyCode::Up), PickerEvent::Moved));
    assert_eq!(picker.index(), 0);
    picker.on_key(KeyCode::Down);
    picker.on_key(KeyCode::Down);
    picker.on_key(KeyCode::Down);
    assert_eq!(picker.index(), 2);
    assert!(matches!(
        picker.on_key(KeyCode::Right),
        PickerEvent::Activated(2)
    ));
}

#[test]
fn enter_activates_the_selected_row() {
    let mut picker = ListPicker::with_selected(3, 1);
    assert!(matches!(
        picker.on_key(KeyCode::Enter),
        PickerEvent::Activated(1)
    ));
    // Right and enter share activation semantics.
    let mut picker = ListPicker::with_selected(3, 1);
    assert!(matches!(
        picker.on_key(KeyCode::Right),
        PickerEvent::Activated(1)
    ));
}

#[test]
fn enter_on_an_empty_list_never_activates() {
    let mut picker = ListPicker::new(0);
    assert!(matches!(picker.on_key(KeyCode::Enter), PickerEvent::Moved));
}

#[test]
fn preselection_and_shrink_stay_in_bounds() {
    let picker = ListPicker::with_selected(3, 9);
    assert_eq!(picker.index(), 2);
    let mut picker = ListPicker::with_selected(3, 2);
    picker.set_len(1);
    assert_eq!(picker.index(), 0);
}

#[test]
fn move_by_pages_and_clamps_like_arrow_navigation() {
    let mut picker = ListPicker::with_selected(12, 0);
    picker.move_by(5);
    assert_eq!(picker.index(), 5);
    picker.move_by(5);
    assert_eq!(picker.index(), 10);
    // Past the end clamps to the last row; past the top clamps to 0.
    picker.move_by(99);
    assert_eq!(picker.index(), 11);
    picker.move_by(-99);
    assert_eq!(picker.index(), 0);
}

#[test]
fn other_keys_are_ignored() {
    let mut picker = ListPicker::new(2);
    assert!(matches!(
        picker.on_key(KeyCode::Char('x')),
        PickerEvent::Ignored
    ));
}

const ACTIONS: &[PickerAction] = &[PickerAction {
    key: 'd',
    label: "delete",
}];

#[test]
fn space_arms_and_the_action_key_fires() {
    let mut picker = ListPicker::with_selected(3, 1).actions(ACTIONS);
    assert!(matches!(
        picker.on_key(KeyCode::Char(' ')),
        PickerEvent::Moved
    ));
    assert!(matches!(
        picker.on_key(KeyCode::Char('d')),
        PickerEvent::Action { key: 'd', row: 1 }
    ));
    // Disarmed again: 'd' is no longer live.
    assert!(matches!(
        picker.on_key(KeyCode::Char('d')),
        PickerEvent::Ignored
    ));
}

#[test]
fn any_other_key_disarms_without_selecting() {
    let mut picker = ListPicker::new(3).actions(ACTIONS);
    picker.on_key(KeyCode::Char(' '));
    assert!(matches!(picker.on_key(KeyCode::Enter), PickerEvent::Moved));
    // The stray enter neither selected nor fired an action; normal
    // navigation resumes.
    assert!(matches!(
        picker.on_key(KeyCode::Enter),
        PickerEvent::Activated(0)
    ));
}

#[test]
fn space_is_ignored_without_actions_or_rows() {
    let mut picker = ListPicker::new(3);
    assert!(matches!(
        picker.on_key(KeyCode::Char(' ')),
        PickerEvent::Ignored
    ));
    let mut picker = ListPicker::new(0).actions(ACTIONS);
    assert!(matches!(
        picker.on_key(KeyCode::Char(' ')),
        PickerEvent::Ignored
    ));
}

#[test]
fn styled_header_keeps_its_spans_and_the_windowed_position() {
    let t = theme();
    let picker = ListPicker::with_selected(3, 2);
    let header = Line::from(vec![
        Span::styled("Done", t.strong),
        Span::styled(" / All · esc close", t.dim),
    ]);
    let lines = picker.windowed_table_lines_styled(
        header,
        ["a", "b", "c"].map(|s| [s.to_string()]),
        [(0, usize::MAX)],
        40,
        2,
    );
    let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.starts_with("  Done / All · esc close"), "{text:?}");
    assert!(text.ends_with("3/3"), "{text:?}");
    // The same right anchor the plain header uses: two cells short of the width.
    assert_eq!(view::cell_width(&text), 38);
    assert_eq!(lines[0].spans[1].content, "Done");
    assert_eq!(lines[0].spans[1].style, t.strong);
    assert_eq!(lines.len(), 4, "header, spacer, two visible rows");
}

#[test]
fn armed_header_shows_the_action_strip() {
    let mut picker = ListPicker::new(2).actions(ACTIONS);
    let text = |p: &ListPicker| -> String {
        p.lines("Header · esc close", ["a".into(), "b".into()], 120)[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    };
    assert_eq!(text(&picker), "  Header · esc close · space actions");
    picker.on_key(KeyCode::Char(' '));
    assert_eq!(
        text(&picker),
        "  actions: [d] delete · any other key cancels"
    );
}

#[test]
fn lines_mark_the_selection() {
    let picker = ListPicker::with_selected(2, 1);
    let lines = picker.lines("Header · esc close", ["a".into(), "b".into()], 80);
    let text: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert_eq!(text[0], "  Header · esc close");
    assert!(text[2].contains("  a"), "unselected row: {}", text[2]);
    assert!(text[3].contains("▸ b"), "selected row: {}", text[3]);
}

#[test]
fn windowed_lines_move_the_window_only_when_the_cursor_leaves_it() {
    let mut picker = ListPicker::with_selected(20, 0);
    let rows = || (1..=20).map(|n| format!("row {n}"));
    let text = |lines: Vec<Line<'_>>| -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    };
    // Moving down inside the window leaves it where it is.
    picker.on_key(KeyCode::Down);
    let shown = text(picker.windowed_lines("Catalog", rows(), 80, 5));
    assert!(shown[2].contains("row 1"), "{shown:?}");
    assert!(shown[3].contains("▸ row 2"), "{shown:?}");
    // Leaving the window at the foot scrolls by one row, not a page.
    for _ in 0..4 {
        picker.on_key(KeyCode::Down);
    }
    let shown = text(picker.windowed_lines("Catalog", rows(), 80, 5));
    assert!(shown[2].contains("row 2"), "{shown:?}");
    assert!(shown[6].contains("▸ row 6"), "{shown:?}");
    // Coming back up keeps the same window until the cursor leaves it.
    picker.on_key(KeyCode::Up);
    let shown = text(picker.windowed_lines("Catalog", rows(), 80, 5));
    assert!(shown[2].contains("row 2"), "{shown:?}");
    assert!(shown[5].contains("▸ row 5"), "{shown:?}");
}

#[test]
fn table_columns_after_the_first_are_dim_and_the_cursor_row_is_select() {
    let picker = ListPicker::with_selected(2, 1);
    let rows = [
        ["name".into(), "description".into()],
        ["other".into(), "text".into()],
    ];
    let lines = picker.table_lines("Catalog", rows, [(0, 10), (0, usize::MAX)], 80);
    let t = theme();
    // Unselected: marker dim, name plain, description dim.
    assert_eq!(lines[2].spans[0].style, t.dim);
    assert_eq!(lines[2].spans[1].style, Style::default());
    assert_eq!(lines[2].spans[2].style, t.dim);
    // Selection emphasizes the primary cell without flattening metadata.
    assert_eq!(lines[3].spans[0].style, t.select);
    assert_eq!(
        lines[3].spans[1].style,
        t.select.add_modifier(ratatui::style::Modifier::BOLD)
    );
    assert_eq!(lines[3].spans[2].style, t.dim);
}

#[test]
fn windowed_lines_keep_a_large_list_cursor_visible_and_show_position() {
    let picker = ListPicker::with_selected(20, 12);
    let rows = (1..=20).map(|n| format!("row {n}"));
    let lines = picker.windowed_lines("Catalog", rows, 80, 5);
    let text: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();
    assert!(text[0].contains("13/20"), "{}", text[0]);
    assert_eq!(text.len(), 7);
    assert!(text.iter().any(|line| line.contains("▸ row 13")));
    assert!(!text.iter().any(|line| line.contains("row 8")));
}

#[test]
fn table_lines_align_columns_and_cap_long_cells() {
    let picker = ListPicker::with_selected(2, 1);
    let rows = [
        ["short".into(), "first description".into()],
        ["a-very-long-name".into(), "second description".into()],
    ];
    let lines = picker.table_lines("Catalog", rows, [(0, 10), (0, usize::MAX)], 80);
    let text: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();

    let first_column = text[2][..text[2].find("first description").unwrap()]
        .chars()
        .count();
    let second_column = text[3][..text[3].find("second description").unwrap()]
        .chars()
        .count();
    assert_eq!(first_column, second_column);
    assert!(text[3].contains("a-very-lo…"), "{}", text[3]);
    assert!(text[3].contains("▸ "), "{}", text[3]);
}

#[test]
fn an_uncapped_first_column_yields_to_the_columns_after_it() {
    let picker = ListPicker::with_selected(2, 0);
    let rows = [
        [
            "a task description far longer than the row can hold".into(),
            "running · 8.1s".into(),
        ],
        ["short".into(), "done · 3.2s".into()],
    ];
    let lines = picker.table_lines("Agents", rows, [(14, usize::MAX), (0, usize::MAX)], 48);
    let text: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();
    assert!(text[2].ends_with("running · 8.1s"), "{}", text[2]);
    assert!(text[2].contains('…'), "{}", text[2]);
    assert!(text[3].ends_with("done · 3.2s"), "{}", text[3]);
    assert!(lines.iter().all(|line| line.width() <= 48));
}

#[test]
fn windowed_table_lines_keep_columns_stable_across_pages() {
    let picker = ListPicker::with_selected(12, 10);
    let rows = (0..12).map(|index| {
        [
            if index == 0 {
                "widest-name".to_string()
            } else {
                format!("s{index}")
            },
            format!("description {index}"),
        ]
    });
    let lines = picker.windowed_table_lines("Catalog", rows, [(0, 20), (0, usize::MAX)], 80, 5);
    let selected: String = lines
        .iter()
        .find(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("description 10"))
        })
        .unwrap()
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();

    let detail_column = selected[..selected.find("description 10").unwrap()]
        .chars()
        .count();
    assert_eq!(detail_column, 17, "{selected}");
    assert!(selected.contains("▸ s10"), "{selected}");
    assert!(lines[0]
        .spans
        .iter()
        .any(|span| span.content.contains("11/12")));
}

#[test]
fn single_line_entries_scroll_by_agent_and_keep_selection_visible() {
    let mut picker = ListPicker::with_selected(12, 9);
    let rows: Vec<_> = (0..12)
        .map(|id| {
            vec![
                Span::raw(format!("Agent #{id}")),
                Span::raw(" · running · 2s"),
            ]
        })
        .collect();
    for height in [0, 1, 2, 3, 4, 8, 12] {
        let lines = picker.cached_entry_lines(Line::from("Agents"), &rows, 30, height);
        assert!(lines.len() <= height);
        assert!(lines.iter().all(|line| line.width() <= 30));
        if height >= 3 {
            assert!(lines
                .iter()
                .any(|line| line.to_string().contains("Agent #9 · running · 2s")));
        }
    }
    picker.move_by(-1);
    let lines = picker.cached_entry_lines(Line::from("Agents"), &rows, 30, 8);
    assert!(lines
        .iter()
        .any(|line| line.to_string().contains("Agent #8")));
    assert_eq!(picker.index(), 8);
}
