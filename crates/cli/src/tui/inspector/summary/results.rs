fn append_shell_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    for (field, label, style) in [
        ("stdout", "stdout", Style::default()),
        ("stderr", "stderr", theme().error),
    ] {
        let text = string(output, field);
        if text.trim().is_empty() {
            continue;
        }
        let (preview, omitted) = window_text(text);
        let mut section = section(format!("{label} · {}", line_label(text)));
        section.extend(inspector_text(&preview, width, style));
        if omitted {
            section.push(Line::from(Span::styled(
                "    output preview shortened",
                theme().dim,
            )));
        }
        section.append_to(lines);
    }
    let exit = output
        .get("exitCode")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let mut result = section(format!("result · exit {exit}"));
    if string(output, "stdout").is_empty() && string(output, "stderr").is_empty() {
        result.extend(inspector_text(
            "Command finished without output",
            width,
            theme().dim,
        ));
    }
    result.append_to(lines);
}
fn append_file_result(
    lines: &mut Vec<Line<'static>>,
    tool: &ToolActivity,
    output: &Value,
    width: usize,
) {
    let content = string(output, "content");
    let path = string(&tool.input, "path");
    let language = language_for_path(path).unwrap_or("text");
    let bytes = output
        .get("bytes")
        .and_then(Value::as_u64)
        .unwrap_or(content.len() as u64);
    let mut source = section(format!(
        "source · {language} · {} · {}",
        line_label(content),
        inspector_size_label(bytes)
    ));
    if content.is_empty() {
        source.extend(inspector_text("This file is empty", width, theme().dim));
    } else {
        let (preview, omitted) = limit_inspector_preview(content);
        source.extend(
            CodePreview {
                text: &preview,
                language,
                width,
                indent: INSPECTOR_BODY_INDENT,
                plain_style: Style::default(),
            }
            .lines(),
        );
        if omitted {
            source.push(Line::from(Span::styled(
                format!("{INSPECTOR_BODY_INDENT}source preview shortened"),
                theme().dim,
            )));
        }
    }
    source.append_to(lines);
}

fn append_mutation_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let mut rows = Vec::new();
    if let Some(path) = output.get("path").and_then(Value::as_str) {
        rows.push(("path", path.to_owned()));
    }
    if let Some(bytes) = output.get("bytesWritten").and_then(Value::as_u64) {
        rows.push(("written", inspector_size_label(bytes)));
    }
    if let Some(count) = output.get("replacements").and_then(Value::as_u64) {
        rows.push(("replacements", count.to_string()));
    }
    let mut result = section("result");
    result.extend(inspector_fields(rows, width));
    result.append_to(lines);
}

fn append_collection(
    lines: &mut Vec<Line<'static>>,
    output: &Value,
    field: &str,
    label: &str,
    width: usize,
) {
    let Some(items) = output.get(field).and_then(Value::as_array) else {
        append_generic_value(lines, "result", output, width);
        return;
    };
    let mut result = section(format!("{label} · {}", items.len()));
    if items.is_empty() {
        result.extend(inspector_text(&format!("No {label}"), width, theme().dim));
    } else {
        for item in items.iter().take(40) {
            let text = collection_item(item);
            result.push(Line::from(Span::raw(view::truncate_line(
                &format!("    {text}"),
                width,
            ))));
        }
        if items.len() > 40 {
            result.push(Line::from(Span::styled(
                format!("    {} more", items.len() - 40),
                theme().dim,
            )));
        }
    }
    result.append_to(lines);
}

fn append_grep_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let Some(matches) = output.get("matches").and_then(Value::as_array) else {
        append_generic_value(lines, "result", output, width);
        return;
    };
    let mut result = section(format!("matches · {}", matches.len()));
    if matches.is_empty() {
        result.extend(inspector_text("No matching lines", width, theme().dim));
    } else {
        for item in matches.iter().take(40) {
            let text = match item {
                Value::Object(map) => {
                    let path = map.get("path").and_then(Value::as_str).unwrap_or("");
                    let line = map
                        .get("line")
                        .and_then(Value::as_u64)
                        .map(|n| n.to_string())
                        .unwrap_or_default();
                    let body = map.get("text").and_then(Value::as_str).unwrap_or("");
                    format!("{path}:{line}  {body}")
                }
                _ => compact_value(item),
            };
            result.push(Line::from(Span::raw(view::truncate_line(
                &format!("    {text}"),
                width,
            ))));
        }
    }
    result.append_to(lines);
}

fn append_process_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    if output.get("processes").is_some() {
        append_collection(lines, output, "processes", "processes", width);
        return;
    }
    let state = if output.get("running").and_then(Value::as_bool) == Some(true) {
        "running".to_owned()
    } else if let Some(exit) = output.get("exitCode").and_then(Value::as_i64) {
        format!("exit {exit}")
    } else {
        "stopped".to_owned()
    };
    let mut process = section("process");
    process.extend(inspector_fields(
        [("id", string(output, "id").to_owned()), ("state", state)]
            .into_iter()
            .filter(|(_, value)| !value.is_empty()),
        width,
    ));
    process.append_to(lines);
    let detail = string(output, "output");
    if !detail.is_empty() {
        let (preview, omitted) = window_text(detail);
        text_section(lines, "output", &preview, width, Style::default());
        if omitted {
            text_section(
                lines,
                "partial result",
                "Output preview shortened",
                width,
                theme().dim,
            );
        }
    }
}

fn append_ask(lines: &mut Vec<Line<'static>>, value: &Value, label: &str, width: usize) {
    let Some(topics) = value.get("topics").and_then(Value::as_array) else {
        append_generic_value(lines, label, value, width);
        return;
    };
    let mut section = section(format!("{label} · {} topics", topics.len()));
    for topic in topics.iter().take(3) {
        let title = ["topic", "title", "question"]
            .into_iter()
            .find_map(|field| topic.get(field).and_then(Value::as_str))
            .unwrap_or("Topic");
        section.push(Line::from(Span::styled(
            view::truncate_line(&format!("    {title}"), width),
            theme().strong,
        )));
        if let Some(questions) = topic.get("questions").and_then(Value::as_array) {
            for question in questions.iter().take(4) {
                let text = string(question, "question");
                if !text.is_empty() {
                    section.push(Line::from(Span::raw(view::truncate_line(
                        &format!("      {text}"),
                        width,
                    ))));
                }
            }
        }
        if let Some(answers) = topic.get("answers").and_then(Value::as_array) {
            for answer in answers.iter().take(4) {
                let values = answer
                    .get("values")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                if !values.is_empty() {
                    section.push(Line::from(Span::raw(view::truncate_line(
                        &format!("      ✓ {values}"),
                        width,
                    ))));
                }
            }
        }
        let context = topic
            .get("additionalContext")
            .or_else(|| topic.get("additional_context"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !context.is_empty() {
            section.push(Line::from(Span::raw(view::truncate_line(
                &format!("      {context}"),
                width,
            ))));
        }
    }
    section.append_to(lines);
}

fn append_todos(lines: &mut Vec<Line<'static>>, value: &Value, width: usize) {
    let Some(items) = value.get("todos").and_then(Value::as_array) else {
        return;
    };
    if items.is_empty() {
        text_section(lines, "todo", "No active tasks", width, theme().dim);
        return;
    }
    let owned: Vec<(String, ProgressState)> = items
        .iter()
        .take(20)
        .map(|item| {
            let content = string(item, "content").to_owned();
            let state = match string(item, "status") {
                "completed" => ProgressState::Completed,
                "in_progress" => ProgressState::Active,
                _ => ProgressState::Pending,
            };
            (content, state)
        })
        .collect();
    let refs: Vec<_> = owned
        .iter()
        .map(|(content, state)| ProgressItem {
            content,
            state: *state,
        })
        .collect();
    lines.extend(progress_list("todo", &refs, width));
    if items.len() > owned.len() {
        lines.push(Line::from(Span::styled(
            format!("    {} more tasks", items.len() - owned.len()),
            theme().dim,
        )));
    }
}

fn append_kernel_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let state = string(output, "state");
    let mut result = section(format!(
        "result · {}",
        if state.is_empty() { "complete" } else { state }
    ));
    let text = string(output, "output");
    if text.is_empty() {
        result.extend(inspector_text(
            "Execution completed without printed output",
            width,
            theme().dim,
        ));
    } else {
        let (preview, omitted) = limit_inspector_preview(text);
        result.extend(inspector_text(&preview, width, Style::default()));
        if omitted {
            result.push(Line::from(Span::styled(
                "    output preview shortened",
                theme().dim,
            )));
        }
    }
    result.append_to(lines);
    for (field, label, style) in [
        ("stderr", "stderr", theme().error),
        ("traceback", "traceback", theme().error),
        ("error", "error", theme().warn),
    ] {
        let text = string(output, field);
        if !text.is_empty() {
            text_section(lines, label, text, width, style);
        }
    }
}

fn append_subagent_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    text_section(
        lines,
        "answer",
        string(output, "answer"),
        width,
        Style::default(),
    );
    if let Some(usage) = output.get("usage") {
        append_generic_value(lines, "usage", usage, width);
    }
}

fn append_web_fetch_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let mut response = section("response");
    response.extend(inspector_fields(
        [
            (
                "status",
                output.get("status").map(compact_value).unwrap_or_default(),
            ),
            ("type", string(output, "contentType").to_owned()),
            ("final url", string(output, "finalUrl").to_owned()),
        ]
        .into_iter()
        .filter(|(_, value)| !value.is_empty()),
        width,
    ));
    response.append_to(lines);
    let content = string(output, "content");
    if !content.is_empty() {
        text_section(lines, "content", content, width, Style::default());
    }
}

fn append_ranked_results(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let field = if output.get("results").is_some() {
        "results"
    } else {
        "tools"
    };
    append_collection(lines, output, field, field, width);
}

fn append_crawl_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let mut result = section("crawl");
    result.extend(inspector_fields(
        [
            ("status", string(output, "status").to_owned()),
            (
                "pages",
                output
                    .get("pagesReturned")
                    .map(compact_value)
                    .unwrap_or_default(),
            ),
            (
                "total",
                output
                    .get("totalPages")
                    .map(compact_value)
                    .unwrap_or_default(),
            ),
        ]
        .into_iter()
        .filter(|(_, value)| !value.is_empty()),
        width,
    ));
    result.append_to(lines);
    if let Some(pages) = output.get("pages").and_then(Value::as_array) {
        let mut list = section(format!("pages · {}", pages.len()));
        for page in pages.iter().take(30) {
            let title = string(page, "title");
            let url = string(page, "url");
            let text = if title.is_empty() { url } else { title };
            list.push(Line::from(Span::raw(view::truncate_line(
                &format!("    {text}"),
                width,
            ))));
        }
        list.append_to(lines);
    }
}

fn append_paged_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let mut page = section("stored result");
    page.extend(inspector_fields(
        [
            ("tool", string(output, "toolName").to_owned()),
            (
                "offset",
                output.get("offset").map(compact_value).unwrap_or_default(),
            ),
            (
                "total",
                output
                    .get("totalChars")
                    .map(compact_value)
                    .unwrap_or_default(),
            ),
        ]
        .into_iter()
        .filter(|(_, value)| !value.is_empty()),
        width,
    ));
    let content = string(output, "content");
    if !content.is_empty() {
        page.extend(inspector_text(content, width, Style::default()));
    }
    page.append_to(lines);
}

fn append_skill_result(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let instructions = string(output, "instructions");
    if !instructions.is_empty() {
        text_section(lines, "instructions", instructions, width, Style::default());
    }
    if let Some(resources) = output.get("resources") {
        append_generic_value(lines, "resources", resources, width);
    }
}

fn append_generic_value(lines: &mut Vec<Line<'static>>, label: &str, value: &Value, width: usize) {
    if let Some(text) = value.as_str() {
        text_section(lines, label, text, width, Style::default());
        return;
    }
    if let Some(map) = value.as_object() {
        let document = ["content", "text", "message", "answer", "markdown"]
            .into_iter()
            .find_map(|key| map.get(key).and_then(Value::as_str).map(|text| (key, text)));
        let mut rows = Vec::new();
        for (key, child) in map {
            if document.is_some_and(|(document_key, _)| document_key == key) {
                continue;
            }
            rows.push((key.as_str(), compact_value(child)));
        }
        if !rows.is_empty() {
            let mut fields = section(label);
            fields.extend(inspector_fields(rows, width));
            fields.append_to(lines);
        }
        if let Some((key, text)) = document {
            text_section(lines, key, text, width, Style::default());
        }
        return;
    }
    if let Some(values) = value.as_array() {
        let mut result = section(format!("{label} · {} items", values.len()));
        for value in values.iter().take(40) {
            result.push(Line::from(Span::raw(view::truncate_line(
                &format!("    {}", compact_value(value)),
                width,
            ))));
        }
        result.append_to(lines);
        return;
    }
    text_section(lines, label, &compact_value(value), width, Style::default());
}

fn append_partial_notice(lines: &mut Vec<Line<'static>>, output: &Value, width: usize) {
    let notice = if output
        .get("droppedBytes")
        .and_then(Value::as_u64)
        .is_some_and(|n| n > 0)
    {
        Some("Older output was dropped")
    } else if output.get("moreOutput").and_then(Value::as_bool) == Some(true) {
        Some("More output is buffered")
    } else if output.get("hasMore").and_then(Value::as_bool) == Some(true) {
        Some("More content is available")
    } else if [
        "truncated",
        "stdoutTruncated",
        "stderrTruncated",
        "_truncated",
    ]
    .into_iter()
    .any(|field| output.get(field).and_then(Value::as_bool) == Some(true))
    {
        Some("The result is partial")
    } else if output.get("timedOut").and_then(Value::as_bool) == Some(true) {
        Some("The operation reached its time limit")
    } else {
        None
    };
    if let Some(notice) = notice {
        text_section(lines, "partial result", notice, width, theme().warn);
    }
}
