use super::markdown::*;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
pub(super) fn is_horizontal_rule(line: &str) -> bool {
    let marks: String = line
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let mut characters = marks.chars();
    let Some(mark) = characters.next() else {
        return false;
    };
    marks.len() >= 3
        && matches!(mark, '-' | '*' | '_')
        && characters.all(|character| character == mark)
}

pub(super) fn ordered_list_item(line: &str) -> Option<(&str, &str)> {
    let dot = line.find('.')?;
    let number = &line[..dot];
    if number.is_empty() || !number.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    let item = line.get(dot + 1..)?.strip_prefix(' ')?;
    Some((number, item))
}

pub(super) fn parse_table_row(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if !trimmed.contains('|') {
        return None;
    }
    let inner = trimmed
        .strip_prefix('|')
        .unwrap_or(trimmed)
        .strip_suffix('|')
        .unwrap_or_else(|| trimmed.strip_prefix('|').unwrap_or(trimmed));
    let cells: Vec<String> = inner
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect();
    (cells.len() >= 2).then_some(cells)
}

pub(super) fn is_table_separator(cells: &[String]) -> bool {
    cells.iter().all(|cell| {
        let marker = cell.trim().trim_start_matches(':').trim_end_matches(':');
        marker.len() >= 3 && marker.chars().all(|character| character == '-')
    })
}

pub(super) fn normalize_table_row(mut row: Vec<String>, columns: usize) -> Vec<String> {
    row.resize(columns, String::new());
    row.truncate(columns);
    row
}

pub(super) fn render_table(
    header: &[String],
    rows: &[Vec<String>],
    width: usize,
    indent: &str,
) -> Vec<Line<'static>> {
    let columns = header.len();
    let separator_width = 3 * columns.saturating_sub(1);
    let available = width
        .saturating_sub(cell_width(indent))
        .saturating_sub(separator_width)
        .max(columns);
    let minimum = if available >= columns * 6 {
        6
    } else {
        (available / columns).max(1)
    };
    let desired: Vec<usize> = (0..columns)
        .map(|column| {
            std::iter::once(&header[column])
                .chain(rows.iter().filter_map(|row| row.get(column)))
                .map(|cell| {
                    inline_spans(cell)
                        .iter()
                        .map(|(text, _)| cell_width(text))
                        .sum()
                })
                .max()
                .unwrap_or(0)
                .max(minimum)
        })
        .collect();
    let mut widths = vec![minimum; columns];
    let mut remaining = available.saturating_sub(minimum * columns);
    while remaining > 0
        && widths
            .iter()
            .zip(&desired)
            .any(|(actual, want)| actual < want)
    {
        for column in 0..columns {
            if remaining == 0 {
                break;
            }
            if widths[column] < desired[column] {
                widths[column] += 1;
                remaining -= 1;
            }
        }
    }

    let mut output = render_table_row(header, &widths, indent, true);
    let rule = widths
        .iter()
        .map(|cell_width| "─".repeat(*cell_width))
        .collect::<Vec<_>>()
        .join("─┼─");
    output.push(Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled(rule, theme().dim),
    ]));
    for row in rows {
        output.extend(render_table_row(row, &widths, indent, false));
    }
    output
}

pub(super) fn render_table_row(
    cells: &[String],
    widths: &[usize],
    indent: &str,
    is_header: bool,
) -> Vec<Line<'static>> {
    let wrapped: Vec<Vec<Vec<Span<'static>>>> = cells
        .iter()
        .zip(widths)
        .map(|(cell, cell_width)| {
            let mut lines = wrap_styled(&inline_spans(cell), *cell_width);
            if is_header {
                for line in &mut lines {
                    for span in line {
                        span.style = span.style.add_modifier(Modifier::BOLD);
                    }
                }
            }
            lines
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
    let mut output = Vec::with_capacity(height);
    for line_index in 0..height {
        let mut spans = vec![Span::raw(indent.to_string())];
        for (column, cell_width) in widths.iter().enumerate() {
            let cell_line = wrapped[column].get(line_index).cloned().unwrap_or_default();
            let content_width = spans_width(&cell_line);
            spans.extend(cell_line);
            spans.push(Span::raw(
                " ".repeat(cell_width.saturating_sub(content_width)),
            ));
            if column + 1 < widths.len() {
                spans.push(Span::styled(" │ ", theme().dim));
            }
        }
        output.push(Line::from(spans));
    }
    output
}

/// Split one paragraph into styled segments: `**bold**` and `` `code` ``.
pub(super) fn inline_spans(text: &str) -> Vec<(String, Style)> {
    let mut segments = Vec::new();
    let mut buffer = String::new();
    let mut bold = false;
    let mut code = false;
    let mut index = 0;

    let flush = |segments: &mut Vec<(String, Style)>, buffer: &mut String, bold, code| {
        if buffer.is_empty() {
            return;
        }
        let mut style = if code { theme().code } else { Style::default() };
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        segments.push((std::mem::take(buffer), style));
    };

    while index < text.len() {
        let rest = &text[index..];
        if rest.starts_with('`') {
            flush(&mut segments, &mut buffer, bold, code);
            code = !code;
            index += 1;
        } else if !code && rest.starts_with("**") {
            flush(&mut segments, &mut buffer, bold, code);
            bold = !bold;
            index += 2;
        } else {
            let character = rest.chars().next().expect("index is within text");
            buffer.push(character);
            index += character.len_utf8();
        }
    }
    flush(&mut segments, &mut buffer, bold, code);
    segments
}

/// Greedy word-wrap over styled segments, preserving each word's style.
pub(super) fn wrap_styled(segments: &[(String, Style)], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_len = 0usize;
    let mut pending_space = false;
    for (text, style) in segments {
        let text = sanitize_cells(text);
        let mut word = String::new();
        for character in text.chars() {
            if character.is_whitespace() {
                if !word.is_empty() {
                    push_wrapped_word(
                        &mut lines,
                        &mut current,
                        &mut current_len,
                        &word,
                        *style,
                        width,
                        pending_space,
                    );
                    word.clear();
                }
                pending_space = true;
            } else {
                word.push(character);
            }
        }
        if !word.is_empty() {
            push_wrapped_word(
                &mut lines,
                &mut current,
                &mut current_len,
                &word,
                *style,
                width,
                pending_space,
            );
            pending_space = false;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(vec![Span::raw("")]);
    }
    lines
}

pub(super) fn push_wrapped_word(
    lines: &mut Vec<Vec<Span<'static>>>,
    current: &mut Vec<Span<'static>>,
    current_len: &mut usize,
    word: &str,
    style: Style,
    width: usize,
    separated: bool,
) {
    let characters: Vec<char> = word.chars().collect();
    let word_len = characters.len();
    let separator_width = usize::from(separated && *current_len > 0);
    if *current_len + separator_width + word_len > width && *current_len > 0 {
        lines.push(std::mem::take(current));
        *current_len = 0;
    }
    if word_len > width {
        for chunk in characters.chunks(width) {
            let chunk_text: String = chunk.iter().collect();
            if chunk.len() == width {
                lines.push(vec![Span::styled(chunk_text, style)]);
            } else {
                current.push(Span::styled(chunk_text, style));
                *current_len = chunk.len();
            }
        }
    } else {
        if separated && *current_len > 0 {
            current.push(Span::raw(" "));
            *current_len += 1;
        }
        current.push(Span::styled(word.to_string(), style));
        *current_len += word_len;
    }
}

/// Which slice of an N-line transcript is visible in a viewport of
/// `height` rows when the user has scrolled `scroll` lines up from the
/// bottom. Returns `(start, end)` indices.
#[cfg(test)]
pub fn scroll_window(len: usize, height: usize, scroll: usize) -> (usize, usize) {
    let max_scroll = len.saturating_sub(height);
    let scroll = scroll.min(max_scroll);
    let end = len - scroll;
    (end.saturating_sub(height), end)
}

/// The full, multi-line rendering of a tool output for on-demand
/// expansion. Shell outputs show stdout/stderr verbatim; everything else
/// pretty-prints as JSON.
pub fn expand_output(name: &str, output: &Value) -> Vec<String> {
    // Raw-text records (thinking blocks, plain string outputs): verbatim.
    if let Some(text) = output.as_str() {
        return text.lines().map(str::to_string).collect();
    }
    if name == "shell" && output.get("stdout").is_some() {
        let mut lines = Vec::new();
        let stdout = output.get("stdout").and_then(Value::as_str).unwrap_or("");
        let stderr = output.get("stderr").and_then(Value::as_str).unwrap_or("");
        lines.extend(stdout.lines().map(str::to_string));
        if !stderr.trim().is_empty() {
            lines.push("stderr:".to_string());
            lines.extend(stderr.lines().map(str::to_string));
        }
        if lines.is_empty() {
            lines.push("(no output)".to_string());
        }
        return lines;
    }
    // File-style outputs: show the content itself, not its JSON encoding.
    if let Some(content) = output.get("content").and_then(Value::as_str) {
        let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
        if output.get("truncated").and_then(Value::as_bool) == Some(true) {
            lines.push("… (truncated)".to_string());
        }
        return lines;
    }
    serde_json::to_string_pretty(output)
        .unwrap_or_else(|_| output.to_string())
        .lines()
        .map(str::to_string)
        .collect()
}

/// Make arbitrary text safe to place in terminal cells: tabs expand to
/// four-column stops, ANSI escape sequences are removed whole, and any
/// other control character is dropped. A raw control byte in a cell is
/// forwarded verbatim to the terminal, which moves the real cursor out
/// of sync with the draw buffer; the differ then never repaints the
/// cells the drift touched and ghost artifacts persist on screen.
pub fn sanitize_cells(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.chars().any(|c| c.is_control() && c != '\n') {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut column = 0usize;
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        match character {
            '\n' => {
                out.push('\n');
                column = 0;
            }
            '\t' => {
                let pad = 4 - column % 4;
                out.extend(std::iter::repeat_n(' ', pad));
                column += pad;
            }
            '\u{1b}' => skip_escape_sequence(&mut chars),
            c if c.is_control() => {}
            c => {
                out.push(c);
                column += 1;
            }
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Consume the rest of an ANSI escape sequence whose ESC was just read:
/// CSI (`ESC [ … final-byte`), OSC (`ESC ] … BEL` or `ESC \`), or a
/// single-character escape.
pub(super) fn skip_escape_sequence(chars: &mut std::str::Chars) {
    match chars.next() {
        Some('[') => {
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
        Some(']') => {
            let mut previous = ' ';
            for c in chars.by_ref() {
                if c == '\u{7}' || (previous == '\u{1b}' && c == '\\') {
                    break;
                }
                previous = c;
            }
        }
        _ => {}
    }
}

/// Terminal cells a string occupies. Every width decision in the renderer
/// goes through here rather than `chars().count()`: a CJK glyph or an
/// emoji is two cells wide, and a row measured in chars overflows the
/// terminal, wraps inside the Paragraph, and throws the transcript's
/// line-based scroll off by one for every such row.
pub fn cell_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// Cells across every span of a rendered line.
pub fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| cell_width(&span.content)).sum()
}

/// Truncate to `max` cells, appending an ellipsis when cut. Multi-line
/// input is flattened to its first line first.
pub fn truncate_line(s: &str, max: usize) -> String {
    let sanitized = sanitize_cells(s);
    let line = sanitized.lines().next().unwrap_or("").trim_end();
    if cell_width(line) <= max {
        return line.to_string();
    }
    let mut out = take_cells(line, max.saturating_sub(1));
    out.push('…');
    out
}

/// The longest prefix of `s` that fits in `max` cells.
pub fn take_cells(s: &str, max: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for c in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > max {
            break;
        }
        used += w;
        out.push(c);
    }
    out
}

/// The transcript line announcing a tool call, e.g. `shell $ cargo test`
/// or `read_file src/main.rs`.
pub(super) fn truncate_styled_line(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    if spans_width(&spans) <= max {
        return spans;
    }

    let mut remaining = max.saturating_sub(1);
    let mut output = Vec::new();
    for span in spans {
        if remaining == 0 {
            break;
        }
        let span_width = cell_width(&span.content);
        if span_width <= remaining {
            remaining -= span_width;
            output.push(span);
            continue;
        }
        let clipped = take_cells(&span.content, remaining);
        output.push(Span::styled(clipped, span.style));
        break;
    }
    let ellipsis_style = output.last().map(|span| span.style).unwrap_or_default();
    output.push(Span::styled("…", ellipsis_style));
    output
}
