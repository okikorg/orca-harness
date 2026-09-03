#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::{tool_call_line, tool_result_summary};
    use serde_json::json;

    #[test]
    fn default_view_theme_uses_the_colored_palette() {
        // Palettes are pure; the active theme is process-global and can be
        // switched by the TUI, so assert on the palette itself rather than
        // the global (tests run in parallel and may switch it).
        let t = theme_for(ThemeName::Default);
        assert_eq!(t.accent.fg, Some(PRIMARY));
        assert_eq!(t.code.fg, Some(PRIMARY));
    }

    #[test]
    fn orca_theme_uses_the_mint_and_paper_palette() {
        let t = theme_for(ThemeName::Orca);
        assert_eq!(t.dim.fg, Some(Color::Rgb(127, 127, 127)));
        assert_eq!(t.accent.fg, Some(Color::Rgb(89, 224, 154)));
        assert_eq!(t.strong.fg, Some(Color::Rgb(244, 244, 244)));
        assert_eq!(t.success.fg, Some(Color::Rgb(89, 224, 154)));
    }

    #[test]
    fn truncate_flattens_and_cuts_with_ellipsis() {
        assert_eq!(truncate_line("hello", 10), "hello");
        assert_eq!(truncate_line("hello world", 8), "hello w…");
        assert_eq!(truncate_line("line one\nline two", 20), "line one");
    }

    #[test]
    fn sanitize_expands_tabs_to_stops_and_drops_controls() {
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
        assert_eq!(sanitize_cells("\u{1b}[31mred\u{1b}[0m plain"), "red plain");
        assert_eq!(sanitize_cells("\u{1b}]0;title\u{7}body"), "body");
        assert_eq!(sanitize_cells("dangling\u{1b}"), "dangling");
    }

    #[test]
    fn truncate_line_never_passes_control_bytes_through() {
        assert_eq!(truncate_line("205M\t/tmp", 20), "205M    /tmp");
        assert_eq!(truncate_line("\u{1b}[1mbold\u{1b}[0m", 20), "bold");
    }

    #[test]
    fn code_lines_render_tabs_as_spaces_not_raw_cells() {
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

    #[test]
    fn shell_calls_show_the_command() {
        let line = tool_call_line("shell", &json!({"command": "cargo test --workspace"}));
        assert_eq!(line, "shell $ cargo test --workspace");
    }

    #[test]
    fn path_tools_show_the_path() {
        let line = tool_call_line("read_file", &json!({"path": "src/main.rs"}));
        assert_eq!(line, "read_file src/main.rs");
        let line = tool_call_line("edit_file", &json!({"path": "a.rs", "find": "x"}));
        assert_eq!(line, "edit_file a.rs");
    }

    #[test]
    fn batch_mutations_show_compact_scope() {
        assert_eq!(
            tool_call_line(
                "multi_edit",
                &json!({"edits": [
                    {"path": "a.rs", "old": "a", "new": "b"},
                    {"path": "b.rs", "old": "c", "new": "d"}
                ]})
            ),
            "multi_edit 2 edits · 2 files"
        );
        assert_eq!(
            tool_call_line(
                "apply_patch",
                &json!({"patch": "*** Begin Patch\n*** Update File: a.rs\n@@\n-a\n+b\n*** Add File: b.rs\n+x\n*** End Patch"})
            ),
            "apply_patch a.rs · 2 files"
        );
    }

    #[test]
    fn grep_shows_the_query() {
        let line = tool_call_line("grep", &json!({"query": "fn main", "path": "."}));
        assert_eq!(line, "grep 'fn main'", "the workspace root is implied");
        let line = tool_call_line("grep", &json!({"query": "fn main", "path": "src/tui"}));
        assert_eq!(line, "grep 'fn main' in src/tui");
    }

    #[test]
    fn skill_and_glob_calls_show_their_useful_arguments() {
        assert_eq!(
            tool_call_line("skill", &json!({"name": "dry-yagni"})),
            "skill dry-yagni"
        );
        assert_eq!(
            tool_call_line(
                "skill",
                &json!({"name": "dry-yagni", "resource": "references/checklist.md"})
            ),
            "skill dry-yagni · references/checklist.md"
        );
        assert_eq!(
            tool_call_line("glob", &json!({"path": "", "pattern": ".orca/**/*.md"})),
            "glob .orca/**/*.md"
        );
        assert_eq!(
            tool_call_line("glob", &json!({"path": "src", "pattern": "*.rs"})),
            "glob src/*.rs"
        );
    }

    #[test]
    fn agent_and_web_calls_show_intent_not_json() {
        assert_eq!(
            tool_call_line("subagent", &json!({"task": "Review the renderer"})),
            "subagent Review the renderer"
        );
        assert_eq!(
            tool_call_line("web_fetch", &json!({"url": "https://example.com/docs"})),
            "web_fetch https://example.com/docs"
        );
        assert_eq!(
            tool_call_line("web_search", &json!({"query": "ratatui rendering"})),
            "web_search 'ratatui rendering'"
        );
    }

    #[test]
    fn process_calls_show_actions_instead_of_json() {
        assert_eq!(
            tool_call_line(
                "process",
                &json!({"action": "spawn", "command": "python3 -u worker.py"})
            ),
            "process spawn $ python3 -u worker.py"
        );
        assert_eq!(
            tool_call_line(
                "process",
                &json!({"action": "poll", "id": "p1", "waitMs": 2000})
            ),
            "process poll p1 · wait 2s"
        );
        assert_eq!(
            tool_call_line(
                "process",
                &json!({"action": "write", "id": "p1", "input": "continue\n"})
            ),
            "process write p1 “continue”"
        );
        assert_eq!(
            tool_call_line("process", &json!({"action": "kill", "id": "p1"})),
            "process kill p1"
        );
    }

    #[test]
    fn unknown_tools_show_their_first_text_field_not_json() {
        let line = tool_call_line("deploy", &json!({"env": "prod"}));
        assert_eq!(line, "deploy prod");
        let line = tool_call_line("deploy", &json!({"replicas": 3, "wait": true}));
        assert_eq!(line, "deploy replicas wait");
        assert_eq!(tool_result_summary("deploy", &json!({"ok": true}), false), "ok");
    }

    #[test]
    fn shell_results_show_exit_and_first_output_line() {
        let out =
            json!({"stdout": "ok 12 tests\nmore", "stderr": "", "exitCode": 0, "success": true});
        assert_eq!(
            tool_result_summary("shell", &out, false),
            "exit 0 · ok 12 tests"
        );
        let out =
            json!({"stdout": "", "stderr": "boom: bad flag", "exitCode": 2, "success": false});
        assert_eq!(
            tool_result_summary("shell", &out, false),
            "exit 2 · boom: bad flag"
        );
    }

    #[test]
    fn file_results_summarize_by_shape() {
        let out = json!({"content": "abc", "bytes": 3, "truncated": false});
        assert_eq!(
            tool_result_summary("read_file", &out, false),
            "read 3 bytes"
        );
        let out = json!({"path": "a.rs", "bytesWritten": 42});
        assert_eq!(
            tool_result_summary("write_file", &out, false),
            "wrote 42 bytes"
        );
        let out = json!({"path": "a.rs", "replacements": 2});
        assert_eq!(
            tool_result_summary("edit_file", &out, false),
            "2 replacements"
        );
        assert_eq!(
            tool_result_summary(
                "multi_edit",
                &json!({"editsApplied": 3, "filesChanged": 2}),
                false
            ),
            "3 edits · 2 files"
        );
        assert_eq!(
            tool_result_summary(
                "apply_patch",
                &json!({"filesChanged": 3, "added": 1, "updated": 2, "deleted": 0}),
                false
            ),
            "3 files · +1 ~2 -0"
        );
        let out = json!({"path": ".", "entries": ["a", "b"]});
        assert_eq!(tool_result_summary("list_dir", &out, false), "2 entries");
    }

    #[test]
    fn skill_and_glob_results_are_semantic_not_json() {
        assert_eq!(
            tool_result_summary(
                "skill",
                &json!({"name": "dry-yagni", "instructions": "# DRY"}),
                false
            ),
            "loaded dry-yagni"
        );
        assert_eq!(
            tool_result_summary(
                "glob",
                &json!({"matches": ["a.md", "b.md"], "truncated": false}),
                false
            ),
            "2 matches"
        );
        assert_eq!(
            tool_result_summary(
                "glob",
                &json!({"matches": ["a.md", "b.md"], "truncated": true}),
                false
            ),
            "2+ matches"
        );
        assert_eq!(
            tool_result_summary(
                "grep",
                &json!({"matches": [{"path": "a.rs", "line": 1}]}),
                false
            ),
            "1 match"
        );
    }

    #[test]
    fn agent_and_web_results_are_semantic_not_json() {
        assert_eq!(
            tool_result_summary(
                "subagent",
                &json!({"termination": "completed", "steps": 3, "toolCalls": 1}),
                false
            ),
            "completed · 3 steps · 1 tool"
        );
        assert_eq!(
            tool_result_summary(
                "web_fetch",
                &json!({"status": 200, "contentType": "text/html; charset=utf-8", "truncated": false}),
                false
            ),
            "status 200 · text/html"
        );
        assert_eq!(
            tool_result_summary("web_search", &json!({"results": [{}, {}]}), false),
            "2 results"
        );
        assert_eq!(
            tool_result_summary(
                "web_crawl",
                &json!({"pagesReturned": 2, "status": "completed", "timedOut": false}),
                false
            ),
            "2 pages · completed"
        );
    }

    #[test]
    fn process_results_show_identity_state_and_first_output() {
        let running = json!({
            "id": "p1",
            "output": "ready\nsecond line",
            "running": true,
            "exitCode": null,
            "moreOutput": false
        });
        assert_eq!(
            tool_result_summary("process", &running, false),
            "p1 · running · ready"
        );

        let exited = json!({
            "id": "p1",
            "output": "",
            "running": false,
            "exitCode": 0,
            "moreOutput": false
        });
        assert_eq!(
            tool_result_summary("process", &exited, false),
            "p1 · exit 0"
        );
        assert_eq!(
            tool_result_summary("process", &json!({"processes": []}), false),
            "no processes"
        );
    }

    fn flat(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn markdown_plain_text_wraps_with_indent() {
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
        let lines = markdown_lines("before\n---\nafter", 24, "  ");
        let texts: Vec<String> = lines.iter().map(flat).collect();

        assert_eq!(
            texts,
            vec!["  before", "  ──────────────────────", "  after"]
        );
    }

    #[test]
    fn fenced_diagrams_are_not_rendered_in_italics() {
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
        assert_eq!(scroll_window(100, 20, 0), (80, 100));
        // Shorter transcript than the viewport: show everything.
        assert_eq!(scroll_window(5, 20, 0), (0, 5));
        assert_eq!(scroll_window(0, 20, 0), (0, 0));
    }

    #[test]
    fn scroll_window_moves_up_and_clamps_at_the_top() {
        assert_eq!(scroll_window(100, 20, 30), (50, 70));
        // Scrolling past the top pins the first page.
        assert_eq!(scroll_window(100, 20, 500), (0, 20));
    }

    #[test]
    fn expand_shell_output_shows_streams_verbatim() {
        let out = json!({"stdout": "line one\nline two", "stderr": "warn: x", "exitCode": 0});
        let lines = expand_output("shell", &out);
        assert_eq!(lines, vec!["line one", "line two", "stderr:", "warn: x"]);
    }

    #[test]
    fn expand_file_content_shows_verbatim_lines() {
        let out = json!({"content": "fn main() {\n    run();\n}", "bytes": 25, "truncated": false});
        let lines = expand_output("read_file", &out);
        assert_eq!(lines, vec!["fn main() {", "    run();", "}"]);
    }

    #[test]
    fn expand_bare_string_output_is_verbatim() {
        let out = json!("First I read the file.\nThen I ran the tests.");
        let lines = expand_output("thinking", &out);
        assert_eq!(
            lines,
            vec!["First I read the file.", "Then I ran the tests."]
        );
    }

    #[test]
    fn expand_generic_output_pretty_prints_json() {
        let out = json!({"entries": ["a", "b"]});
        let lines = expand_output("list_dir", &out);
        let joined = lines.join("\n");
        assert!(joined.contains("\"entries\""));
        assert!(lines.len() >= 3, "pretty multi-line: {lines:?}");
    }

    #[test]
    fn errors_show_the_message() {
        let out = json!({"error": "no such file: a.rs"});
        assert_eq!(
            tool_result_summary("read_file", &out, true),
            "error: no such file: a.rs"
        );
        let out = json!("denied by user");
        assert_eq!(
            tool_result_summary("shell", &out, true),
            "error: denied by user"
        );
    }
}
