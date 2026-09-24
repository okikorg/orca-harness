#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::{tool_call_line, tool_result_summary};
    use serde_json::json;

    #[test]
    fn default_view_theme_uses_the_colored_palette() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        // Palettes are pure; the active theme is process-global and can be
        // switched by the TUI, so assert on the palette itself rather than
        // the global (tests run in parallel and may switch it).
        let t = theme_for(ThemeName::Default);
        assert_eq!(t.accent.fg, Some(PRIMARY));
        assert_eq!(t.code.fg, Some(PRIMARY));
    }

    #[test]
    fn orca_theme_uses_the_mint_and_paper_palette() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let t = theme_for(ThemeName::Orca);
        assert_eq!(t.dim.fg, Some(Color::Rgb(127, 127, 127)));
        assert_eq!(t.accent.fg, Some(Color::Rgb(89, 224, 154)));
        assert_eq!(t.strong.fg, Some(Color::Rgb(244, 244, 244)));
        assert_eq!(t.success.fg, Some(Color::Rgb(89, 224, 154)));
    }

    #[test]
    fn truncate_flattens_and_cuts_with_ellipsis() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        assert_eq!(truncate_line("hello", 10), "hello");
        assert_eq!(truncate_line("hello world", 8), "hello w…");
        assert_eq!(truncate_line("line one\nline two", 20), "line one");
    }

    #[test]
    fn sanitize_expands_tabs_to_stops_and_drops_controls() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        // A raw tab in a cell moves the real terminal cursor to the next
        // tab stop while the draw buffer budgets one column, desyncing
        // the two and leaving ghost cells the differ never repaints.
        assert_eq!(sanitize_cells("205M\t/Users/x"), "205M    /Users/x");
        assert_eq!(sanitize_cells("ab\tc"), "ab  c");
        assert_eq!(sanitize_cells("a\u{8}b\u{7}c\r"), "abc");
        assert_eq!(sanitize_cells("plain text"), "plain text");
        assert_eq!(
            sanitize_cells("keeps\nnewlines\tok"),
            "keeps\nnewlines    ok"
        );
    }

    #[test]
    fn sanitize_strips_ansi_escape_sequences_whole() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        assert_eq!(sanitize_cells("\u{1b}[31mred\u{1b}[0m plain"), "red plain");
        assert_eq!(sanitize_cells("\u{1b}]0;title\u{7}body"), "body");
        assert_eq!(sanitize_cells("dangling\u{1b}"), "dangling");
    }

    #[test]
    fn truncate_line_never_passes_control_bytes_through() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        assert_eq!(truncate_line("205M\t/tmp", 20), "205M    /tmp");
        assert_eq!(truncate_line("\u{1b}[1mbold\u{1b}[0m", 20), "bold");
    }

    #[test]
    fn code_lines_render_tabs_as_spaces_not_raw_cells() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let lines = highlighted_code_lines("205M\t/Users/x\n12K\t/tmp/y\n", "text", 60, "");
        for line in &lines {
            for span in &line.spans {
                assert!(
                    !span.content.contains(|c: char| c.is_control()),
                    "control byte reached a cell: {:?}",
                    span.content
                );
            }
        }
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(joined.contains("205M    /Users/x"));
    }

    include!("tests/tool_output.rs");

    fn flat(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn markdown_plain_text_wraps_with_indent() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let lines = markdown_lines("alpha beta gamma delta", 14, "  ");
        assert!(lines.len() >= 2, "should wrap: {lines:?}");
        for line in &lines {
            let text = flat(line);
            assert!(text.starts_with("  "), "indent kept: {text}");
            assert!(text.chars().count() <= 14, "width respected: {text}");
        }
        let joined: String = lines.iter().map(|l| flat(l) + " ").collect();
        for word in ["alpha", "beta", "gamma", "delta"] {
            assert!(joined.contains(word));
        }
    }

    #[test]
    fn markdown_styles_bold_and_inline_code() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        set_theme(ThemeName::Default);
        let lines = markdown_lines("use **bold** and `code` now", 80, "");
        assert_eq!(lines.len(), 1);
        let bold = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref().trim() == "bold")
            .expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref().trim() == "code")
            .expect("code span");
        assert_eq!(code.style.fg, Some(PRIMARY));
        assert!(!code.style.add_modifier.contains(Modifier::ITALIC));
        let text = flat(&lines[0]);
        assert!(!text.contains("**"), "markers stripped: {text}");
        assert!(!text.contains('`'), "markers stripped: {text}");
    }

    #[test]
    fn markdown_renders_code_fences_as_bordered_verbatim_lines() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let lines = markdown_lines("before\n```rust\nlet x = 1; // long\n```\nafter", 80, "  ");
        let texts: Vec<String> = lines.iter().map(flat).collect();
        assert!(
            texts.iter().any(|t| t == "  │ let x = 1; // long"),
            "{texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("```")),
            "fences dropped: {texts:?}"
        );
    }

    #[test]
    fn rust_fences_render_with_multiple_token_colors() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let source = "pub fn greet(name: &str) -> bool { name == \"orca\" }";
        let lines = markdown_lines(&format!("```rust\n{source}\n```"), 100, "  ");
        let code_line = lines.first().expect("highlighted code line");

        assert_eq!(flat(code_line), format!("  │ {source}"));

        let token_colors: Vec<Color> = code_line
            .spans
            .iter()
            .skip(1)
            .filter_map(|span| span.style.fg)
            .collect();
        assert!(
            token_colors
                .iter()
                .enumerate()
                .any(|(index, color)| token_colors[index + 1..].iter().any(|next| next != color)),
            "expected more than one syntax color: {code_line:?}"
        );
    }

    #[test]
    fn inspector_code_highlights_and_wraps_without_ellipsis() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        set_theme(ThemeName::Default);
        let source = "let deterministic_result = compute_parallel_tool_output();";
        let lines = highlighted_code_lines(source, "rust", 32, "  ");
        assert!(lines.len() > 1, "long code wraps: {lines:?}");
        let text = lines.iter().map(flat).collect::<Vec<_>>().join("\n");
        assert!(
            !text.contains('…'),
            "inspector code is not truncated: {text}"
        );
        let colors: std::collections::HashSet<_> = lines
            .iter()
            .flat_map(|line| line.spans.iter().filter_map(|span| span.style.fg))
            .collect();
        assert!(colors.len() > 1, "syntax colors applied: {lines:?}");
    }

    #[test]
    fn inspector_code_preserves_indentation_and_blank_lines() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        set_theme(ThemeName::Default);
        let source = "fn main() {\n    if ready {\n        run();\n    }\n\n    finish();\n}";
        let rendered = highlighted_code_lines(source, "rust", 80, "  ")
            .iter()
            .map(flat)
            .collect::<Vec<_>>();
        assert_eq!(rendered[1], "  │     if ready {");
        assert_eq!(rendered[2], "  │         run();");
        assert_eq!(rendered[4], "  │ ");
        assert_eq!(rendered[5], "  │     finish();");
    }

    #[test]
    fn monochrome_code_blocks_do_not_emit_token_colors() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let lines = render_code_block(
            &["pub fn greet() -> bool { true }"],
            "rust",
            100,
            "  ",
            &mono_theme(),
        );

        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style.fg.is_none() || span.content.contains('│')),
            "monochrome code must stay colorless: {lines:?}"
        );
    }

    #[test]
    fn truncated_code_does_not_corrupt_following_line_highlights() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let source = [
            "let value = 1; /* a deliberately long comment that closes here */",
            "pub fn next() {}",
        ];
        let narrow = render_code_block(&source, "rust", 30, "  ", &default_theme());
        let wide = render_code_block(&source, "rust", 120, "  ", &default_theme());

        let pub_color = |lines: &[Line<'static>]| {
            lines[1]
                .spans
                .iter()
                .find(|span| span.content.contains("pub"))
                .and_then(|span| span.style.fg)
        };
        assert_eq!(pub_color(&narrow), pub_color(&wide));
        assert!(flat(&narrow[0]).ends_with('…'));
    }

    #[test]
    fn markdown_bullets_and_headers() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let lines = markdown_lines("## Title\n- item one\n* item two", 80, "  ");
        let texts: Vec<String> = lines.iter().map(flat).collect();
        assert!(texts.contains(&"  ## Title".to_string()), "{texts:?}");
        assert!(texts.contains(&"  • item one".to_string()), "{texts:?}");
        assert!(texts.contains(&"  • item two".to_string()), "{texts:?}");
        let title = lines.iter().find(|l| flat(l).contains("Title")).unwrap();
        assert!(title.spans[2].style.add_modifier.contains(Modifier::BOLD));
        let deep = markdown_lines("#### Deep", 80, "");
        assert_eq!(flat(&deep[0]), "### Deep", "levels past three share h3");
    }

    #[test]
    fn markdown_ordered_sections_render_nested_emphasis_without_markers() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let lines = markdown_lines(
            "1. **Agent**\n8. **Host / CLI (`orcacode`)**\n   continued explanation",
            80,
            "  ",
        );
        let texts: Vec<String> = lines.iter().map(flat).collect();

        assert_eq!(texts[0], "  1. Agent");
        assert_eq!(texts[1], "  8. Host / CLI (orcacode)");
        assert_eq!(texts[2], "     continued explanation");
        assert!(
            texts
                .iter()
                .all(|line| !line.contains("**") && !line.contains('`')),
            "markdown markers leaked: {texts:?}"
        );

        let host_line = &lines[1];
        let closing_parenthesis = host_line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == ")")
            .expect("closing parenthesis span");
        assert!(
            closing_parenthesis
                .style
                .add_modifier
                .contains(Modifier::BOLD),
            "bold style should resume after inline code: {host_line:?}"
        );
    }

    #[test]
    fn markdown_horizontal_rule_uses_terminal_rule() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let lines = markdown_lines("before\n---\nafter", 24, "  ");
        let texts: Vec<String> = lines.iter().map(flat).collect();

        assert_eq!(
            texts,
            vec!["  before", "  ──────────────────────", "  after"]
        );
    }

    #[test]
    fn fenced_diagrams_are_not_rendered_in_italics() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let lines = markdown_lines("```text\n┌──────┐\n│ CORE │\n└──────┘\n```", 40, "  ");
        let diagram_spans = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.content.contains("──────") || span.content.contains("CORE"));

        for span in diagram_spans {
            assert!(
                !span.style.add_modifier.contains(Modifier::ITALIC),
                "diagram span should stay upright: {span:?}"
            );
        }
    }

    #[test]
    fn markdown_renders_gfm_tables_without_source_delimiters() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        set_theme(ThemeName::Default);
        let markdown = "| Action | What I can do | How to ask |\n\
                        | --- | --- | --- |\n\
                        | **Run shell** | Execute a command | `shell ls` |\n\
                        | Read a file | Show its contents | `read_file README.md` |";
        let lines = markdown_lines(markdown, 100, "  ");
        let texts: Vec<String> = lines.iter().map(flat).collect();

        assert!(
            texts.iter().any(|line| line.contains("Action")),
            "{texts:?}"
        );
        assert!(
            texts.iter().any(|line| line.contains("Run shell")),
            "{texts:?}"
        );
        assert!(
            texts.iter().all(|line| !line.contains("---")),
            "separator markdown must not leak: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .all(|line| !line.trim().starts_with('|') && !line.trim().ends_with('|')),
            "outer pipe syntax must not leak: {texts:?}"
        );

        let header = lines
            .iter()
            .find(|line| flat(line).contains("Action"))
            .expect("table header");
        assert!(
            header
                .spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD)),
            "header cells should be emphasized: {header:?}"
        );
        for code_word in ["shell", "ls"] {
            let code = lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content.as_ref() == code_word && span.style.fg == Some(PRIMARY))
                .unwrap_or_else(|| panic!("inline code word {code_word}"));
            assert_eq!(code.style.fg, Some(PRIMARY));
        }
    }

    #[test]
    fn markdown_table_wraps_cells_to_the_terminal_width() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let markdown = "| Name | Description |\n\
                        | --- | --- |\n\
                        | renderer | This description is long enough to wrap over several terminal lines |\n\
                        | command | `read_file({\"path\":\"a/very/long/path/to/README.md\"})` |";
        let lines = markdown_lines(markdown, 42, "  ");
        let texts: Vec<String> = lines.iter().map(flat).collect();

        assert!(
            texts.iter().all(|line| line.chars().count() <= 42),
            "table exceeded viewport: {texts:?}"
        );
        let joined = texts.join(" ");
        for word in ["description", "enough", "several", "terminal", "lines"] {
            assert!(joined.contains(word), "missing {word}: {texts:?}");
        }
        assert!(lines.len() >= 4, "long cell should wrap: {texts:?}");
    }

    #[test]
    fn scroll_window_follows_the_bottom_by_default() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        assert_eq!(scroll_window(100, 20, 0), (80, 100));
        // Shorter transcript than the viewport: show everything.
        assert_eq!(scroll_window(5, 20, 0), (0, 5));
        assert_eq!(scroll_window(0, 20, 0), (0, 0));
    }

    #[test]
    fn scroll_window_moves_up_and_clamps_at_the_top() {
        let _theme = crate::tui::THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        assert_eq!(scroll_window(100, 20, 30), (50, 70));
        // Scrolling past the top pins the first page.
        assert_eq!(scroll_window(100, 20, 500), (0, 20));
    }
}
