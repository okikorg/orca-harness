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
