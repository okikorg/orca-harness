//! Pure rendering helpers: turn tool calls and results into the compact
//! one-liners the transcript shows, and assistant markdown into styled
//! lines. No terminal state — unit-testable.

use std::sync::{OnceLock, RwLock};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use syntect_tui::into_span;
use two_face::{
    re_exports::syntect::{
        easy::HighlightLines, highlighting::Theme as SyntaxTheme, parsing::SyntaxSet,
    },
    theme::{EmbeddedLazyThemeSet, EmbeddedThemeName},
};

/// Recognised theme identifiers (plus the default colour theme).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeName {
    Default,
    Mono,
    Dracula,
    SolarizedDark,
    OneDark,
    Monokai,
    Nord,
}

impl ThemeName {
    pub const ALL: [ThemeName; 7] = [
        Self::Default,
        Self::Mono,
        Self::Dracula,
        Self::SolarizedDark,
        Self::OneDark,
        Self::Monokai,
        Self::Nord,
    ];

    /// The CLI-facing identifier, as accepted by `from_str`.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Mono => "mono",
            Self::Dracula => "dracula",
            Self::SolarizedDark => "solarized-dark",
            Self::OneDark => "one-dark",
            Self::Monokai => "monokai",
            Self::Nord => "nord",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "default" | "color" => Some(Self::Default),
            "mono" => Some(Self::Mono),
            "dracula" => Some(Self::Dracula),
            "solarized-dark" | "solarized" => Some(Self::SolarizedDark),
            "one-dark" | "onedark" => Some(Self::OneDark),
            "monokai" => Some(Self::Monokai),
            "nord" => Some(Self::Nord),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Mono => "Mono",
            Self::Dracula => "Dracula",
            Self::SolarizedDark => "Solarized Dark",
            Self::OneDark => "One Dark",
            Self::Monokai => "Monokai",
            Self::Nord => "Nord",
        }
    }
}

/// Named styles used across the UI. Default is the colored theme;
/// `--theme mono` keeps the interface colorless when requested.
pub struct Theme {
    /// Secondary chrome: borders, hints, tool summaries, status line.
    pub dim: Style,
    /// Interactive accents: prompt glyph, tool calls, spinner, banner.
    pub accent: Style,
    /// Emphasis: user prompts, palette selection.
    pub strong: Style,
    /// Approval prompts.
    pub warn: Style,
    pub error: Style,
    /// Additions in diff previews.
    pub success: Style,
    /// Inline code and code-block text.
    pub code: Style,
}

pub fn mono_theme() -> Theme {
    let dim = Style::default().fg(Color::DarkGray);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    Theme {
        dim,
        accent: bold,
        strong: bold,
        warn: bold,
        error: bold,
        success: Style::default(),
        code: Style::default().add_modifier(Modifier::ITALIC),
    }
}

pub fn default_theme() -> Theme {
    Theme {
        dim: Style::default().fg(Color::DarkGray),
        accent: Style::default().fg(Color::Cyan),
        strong: Style::default().add_modifier(Modifier::BOLD),
        warn: Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        error: Style::default().fg(Color::Red),
        success: Style::default().fg(Color::Green),
        code: Style::default().fg(Color::Cyan),
    }
}

// https://draculatheme.com
fn dracula_theme() -> Theme {
    Theme {
        dim: Style::default().fg(Color::Rgb(98, 114, 164)), // comment
        accent: Style::default().fg(Color::Rgb(255, 121, 198)), // pink
        strong: Style::default()
            .fg(Color::Rgb(189, 147, 249))
            .add_modifier(Modifier::BOLD), // purple bold
        warn: Style::default()
            .fg(Color::Rgb(241, 250, 140))
            .add_modifier(Modifier::BOLD), // yellow bold
        error: Style::default().fg(Color::Rgb(255, 85, 85)), // red
        success: Style::default().fg(Color::Rgb(80, 250, 123)), // green
        code: Style::default().fg(Color::Rgb(139, 233, 253)), // cyan
    }
}

// https://ethanschoonover.com/solarized  (dark variant)
fn solarized_dark_theme() -> Theme {
    Theme {
        dim: Style::default().fg(Color::Rgb(131, 148, 150)), // base0
        accent: Style::default().fg(Color::Rgb(38, 139, 210)), // blue
        strong: Style::default()
            .fg(Color::Rgb(147, 161, 161))
            .add_modifier(Modifier::BOLD), // base1 bold
        warn: Style::default()
            .fg(Color::Rgb(181, 137, 0))
            .add_modifier(Modifier::BOLD), // yellow bold
        error: Style::default().fg(Color::Rgb(220, 50, 47)), // red
        success: Style::default().fg(Color::Rgb(133, 153, 0)), // green
        code: Style::default().fg(Color::Rgb(42, 161, 152)), // cyan
    }
}

// Atom One Dark
fn one_dark_theme() -> Theme {
    Theme {
        dim: Style::default().fg(Color::Rgb(92, 99, 112)), // comment
        accent: Style::default().fg(Color::Rgb(97, 175, 239)), // blue
        strong: Style::default()
            .fg(Color::Rgb(229, 192, 123))
            .add_modifier(Modifier::BOLD), // yellow bold
        warn: Style::default()
            .fg(Color::Rgb(209, 154, 102))
            .add_modifier(Modifier::BOLD), // orange bold
        error: Style::default().fg(Color::Rgb(224, 108, 117)), // red
        success: Style::default().fg(Color::Rgb(152, 195, 121)), // green
        code: Style::default().fg(Color::Rgb(86, 182, 194)), // cyan
    }
}

// Monokai
fn monokai_theme() -> Theme {
    Theme {
        dim: Style::default().fg(Color::Rgb(117, 113, 94)), // comment
        accent: Style::default().fg(Color::Rgb(166, 226, 46)), // green-bright
        strong: Style::default()
            .fg(Color::Rgb(249, 38, 114))
            .add_modifier(Modifier::BOLD), // pink bold
        warn: Style::default()
            .fg(Color::Rgb(230, 219, 116))
            .add_modifier(Modifier::BOLD), // yellow bold
        error: Style::default().fg(Color::Rgb(249, 38, 114)), // pink
        success: Style::default().fg(Color::Rgb(166, 226, 46)), // green
        code: Style::default().fg(Color::Rgb(102, 217, 239)), // cyan
    }
}

// Nord
fn nord_theme() -> Theme {
    Theme {
        dim: Style::default().fg(Color::Rgb(76, 86, 106)), // nord3
        accent: Style::default().fg(Color::Rgb(136, 192, 208)), // nord8
        strong: Style::default()
            .fg(Color::Rgb(216, 222, 233))
            .add_modifier(Modifier::BOLD), // nord4 bold
        warn: Style::default()
            .fg(Color::Rgb(235, 203, 139))
            .add_modifier(Modifier::BOLD), // nord13 bold
        error: Style::default().fg(Color::Rgb(191, 97, 106)), // nord11
        success: Style::default().fg(Color::Rgb(163, 190, 140)), // nord14
        code: Style::default().fg(Color::Rgb(136, 192, 208)), // nord8
    }
}

pub fn theme_for(name: ThemeName) -> Theme {
    match name {
        ThemeName::Default => default_theme(),
        ThemeName::Mono => mono_theme(),
        ThemeName::Dracula => dracula_theme(),
        ThemeName::SolarizedDark => solarized_dark_theme(),
        ThemeName::OneDark => one_dark_theme(),
        ThemeName::Monokai => monokai_theme(),
        ThemeName::Nord => nord_theme(),
    }
}

static THEME: RwLock<Option<ThemeName>> = RwLock::new(None);

/// Install the theme at startup. If already set, replaces it (allows
/// runtime switching via `/theme`).
fn install_theme(theme: ThemeName) {
    if let Ok(mut guard) = THEME.write() {
        *guard = Some(theme);
    }
}

pub fn set_theme(theme: ThemeName) {
    install_theme(theme);
}

/// Return the current theme name, or Color if none set.
pub fn theme_name() -> ThemeName {
    THEME
        .read()
        .ok()
        .and_then(|g| *g)
        .unwrap_or(ThemeName::Default)
}

pub fn theme() -> Theme {
    theme_for(theme_name())
}

/// Render assistant markdown into styled, wrapped lines. Handles the
/// common cases models emit: fenced code blocks, inline `code`,
/// **bold**, # headers, - bullets, and GFM tables. Everything else passes
/// through.
pub fn markdown_lines(text: &str, width: usize, indent: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let dim = theme().dim;
    let raw_lines: Vec<&str> = text.split('\n').collect();
    let mut index = 0;
    while index < raw_lines.len() {
        let raw = raw_lines[index];
        let trimmed = raw.trim_start();
        if let Some(info) = trimmed.strip_prefix("```") {
            let language = info.split_whitespace().next().unwrap_or_default();
            let code_start = index + 1;
            let code_end = raw_lines[code_start..]
                .iter()
                .position(|line| line.trim_start().starts_with("```"))
                .map(|offset| code_start + offset)
                .unwrap_or(raw_lines.len());
            out.extend(render_code_block(
                &raw_lines[code_start..code_end],
                language,
                width,
                indent,
                &theme(),
            ));
            index = if code_end < raw_lines.len() {
                code_end + 1
            } else {
                code_end
            };
            continue;
        }
        if let Some(header) = parse_table_row(raw) {
            if let Some(separator) = raw_lines
                .get(index + 1)
                .and_then(|line| parse_table_row(line))
            {
                if separator.len() == header.len() && is_table_separator(&separator) {
                    let mut rows = Vec::new();
                    index += 2;
                    while let Some(row) =
                        raw_lines.get(index).and_then(|line| parse_table_row(line))
                    {
                        rows.push(normalize_table_row(row, header.len()));
                        index += 1;
                    }
                    out.extend(render_table(&header, &rows, width, indent));
                    continue;
                }
            }
        }
        if raw.trim().is_empty() {
            out.push(Line::from(""));
            index += 1;
            continue;
        }
        if is_horizontal_rule(trimmed) {
            out.push(Line::from(vec![
                Span::raw(indent.to_string()),
                Span::styled(
                    "─".repeat(width.saturating_sub(indent.chars().count())),
                    dim,
                ),
            ]));
            index += 1;
            continue;
        }
        if let Some(header) = trimmed
            .strip_prefix('#')
            .map(|h| h.trim_start_matches('#').trim_start())
        {
            out.push(Line::from(vec![
                Span::raw(indent.to_string()),
                Span::styled(
                    header.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]));
            index += 1;
            continue;
        }
        // Bullets keep their nesting depth and get a hanging indent.
        let leading = &raw[..raw.len() - trimmed.len()];
        let ordered = ordered_list_item(trimmed);
        let (body, first_prefix, cont_prefix) = if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            (
                item,
                format!("{indent}{leading}• "),
                format!("{indent}{leading}  "),
            )
        } else if let Some((number, item)) = ordered {
            let marker = format!("{number}. ");
            (
                item,
                format!("{indent}{leading}{marker}"),
                format!("{indent}{leading}{}", " ".repeat(marker.chars().count())),
            )
        } else {
            (
                trimmed,
                format!("{indent}{leading}"),
                format!("{indent}{leading}"),
            )
        };
        let segments = inline_spans(body);
        for (i, spans) in wrap_styled(&segments, width.saturating_sub(cont_prefix.len()).max(8))
            .into_iter()
            .enumerate()
        {
            let prefix = if i == 0 { &first_prefix } else { &cont_prefix };
            let mut line = vec![Span::raw(prefix.clone())];
            line.extend(spans);
            out.push(Line::from(line));
        }
        index += 1;
    }
    out
}

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static SYNTAX_THEMES: OnceLock<EmbeddedLazyThemeSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

fn syntax_theme() -> &'static SyntaxTheme {
    SYNTAX_THEMES
        .get_or_init(two_face::theme::extra)
        .get(EmbeddedThemeName::Base16OceanDark)
}

fn render_code_block(
    lines: &[&str],
    language: &str,
    width: usize,
    indent: &str,
    selected_theme: &Theme,
) -> Vec<Line<'static>> {
    let code_width = width.saturating_sub(indent.chars().count() + 2).max(8);
    let fallback_style = selected_theme.code.remove_modifier(Modifier::ITALIC);
    let fallback = || {
        lines
            .iter()
            .map(|raw| {
                Line::from(vec![
                    Span::styled(format!("{indent}│ "), selected_theme.dim),
                    Span::styled(truncate_line(raw, code_width), fallback_style),
                ])
            })
            .collect()
    };

    if selected_theme.code.fg.is_none() {
        return fallback();
    }
    let language = language.trim().trim_start_matches("language-");
    if language.is_empty() || matches!(language, "text" | "txt" | "plain" | "plaintext") {
        return fallback();
    }
    let Some(syntax) = syntax_set().find_syntax_by_token(language) else {
        return fallback();
    };

    let mut highlighter = HighlightLines::new(syntax, syntax_theme());
    let mut output = Vec::with_capacity(lines.len());
    for raw in lines {
        let code = raw.trim_end();
        let line_with_ending = format!("{code}\n");
        let highlighted = match highlighter.highlight_line(&line_with_ending, syntax_set()) {
            Ok(highlighted) => highlighted,
            Err(_) => return fallback(),
        };
        let mut spans = vec![Span::styled(format!("{indent}│ "), selected_theme.dim)];
        let mut code_spans = Vec::with_capacity(highlighted.len());
        for segment in highlighted {
            let content = segment.1.trim_end_matches(['\r', '\n']);
            if content.is_empty() {
                continue;
            }
            let converted = into_span((segment.0, content));
            match converted {
                Ok(span) => {
                    let mut style = span.style;
                    style.bg = None;
                    code_spans.push(Span::styled(span.content.into_owned(), style));
                }
                Err(_) => code_spans.push(Span::styled(content.to_string(), fallback_style)),
            }
        }
        spans.extend(truncate_styled_line(code_spans, code_width));
        output.push(Line::from(spans));
    }
    output
}

/// Syntax-highlight code for the scrollable inspector. Unlike transcript
/// fences, inspector rows wrap styled segments instead of truncating them.
pub fn highlighted_code_lines(
    source: &str,
    language: &str,
    width: usize,
    indent: &str,
) -> Vec<Line<'static>> {
    let selected_theme = theme();
    let code_width = width.saturating_sub(indent.chars().count() + 2).max(8);
    let fallback_style = selected_theme.code.remove_modifier(Modifier::ITALIC);
    let fallback = || {
        source
            .lines()
            .flat_map(|raw| {
                wrap_styled_verbatim(&[(raw.to_owned(), fallback_style)], code_width)
                    .into_iter()
                    .map(|wrapped| {
                        let mut spans =
                            vec![Span::styled(format!("{indent}│ "), selected_theme.dim)];
                        spans.extend(wrapped);
                        Line::from(spans)
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    if selected_theme.code.fg.is_none() {
        return fallback();
    }
    let language = language.trim().trim_start_matches("language-");
    let Some(syntax) = syntax_set().find_syntax_by_token(language) else {
        return fallback();
    };
    let mut highlighter = HighlightLines::new(syntax, syntax_theme());
    let mut output = Vec::new();
    for raw in source.lines() {
        let line_with_ending = format!("{}\n", raw.trim_end());
        let Ok(highlighted) = highlighter.highlight_line(&line_with_ending, syntax_set()) else {
            return fallback();
        };
        let mut segments = Vec::new();
        for (style, content) in highlighted {
            let content = content.trim_end_matches(['\r', '\n']);
            if content.is_empty() {
                continue;
            }
            let converted = into_span((style, content));
            match converted {
                Ok(span) => {
                    let mut style = span.style;
                    style.bg = None;
                    segments.push((span.content.into_owned(), style));
                }
                Err(_) => segments.push((content.to_string(), fallback_style)),
            }
        }
        for wrapped in wrap_styled_verbatim(&segments, code_width) {
            let mut spans = vec![Span::styled(format!("{indent}│ "), selected_theme.dim)];
            spans.extend(wrapped);
            output.push(Line::from(spans));
        }
    }
    output
}

/// Hard-wrap styled code without collapsing leading or repeated whitespace.
fn wrap_styled_verbatim(segments: &[(String, Style)], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut lines = vec![Vec::new()];
    let mut current_len = 0usize;
    for (text, style) in segments {
        let mut chunk = String::new();
        for character in text.chars() {
            if current_len == width {
                if !chunk.is_empty() {
                    lines
                        .last_mut()
                        .expect("one line exists")
                        .push(Span::styled(std::mem::take(&mut chunk), *style));
                }
                lines.push(Vec::new());
                current_len = 0;
            }
            chunk.push(character);
            current_len += 1;
        }
        if !chunk.is_empty() {
            lines
                .last_mut()
                .expect("one line exists")
                .push(Span::styled(chunk, *style));
        }
    }
    lines
}

fn truncate_styled_line(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(|span| span.content.chars().count()).sum();
    if total <= max {
        return spans;
    }

    let mut remaining = max.saturating_sub(1);
    let mut output = Vec::new();
    for span in spans {
        if remaining == 0 {
            break;
        }
        let span_width = span.content.chars().count();
        if span_width <= remaining {
            remaining -= span_width;
            output.push(span);
            continue;
        }
        let clipped: String = span.content.chars().take(remaining).collect();
        output.push(Span::styled(clipped, span.style));
        break;
    }
    let ellipsis_style = output.last().map(|span| span.style).unwrap_or_default();
    output.push(Span::styled("…", ellipsis_style));
    output
}

fn is_horizontal_rule(line: &str) -> bool {
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

fn ordered_list_item(line: &str) -> Option<(&str, &str)> {
    let dot = line.find('.')?;
    let number = &line[..dot];
    if number.is_empty() || !number.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    let item = line.get(dot + 1..)?.strip_prefix(' ')?;
    Some((number, item))
}

fn parse_table_row(line: &str) -> Option<Vec<String>> {
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

fn is_table_separator(cells: &[String]) -> bool {
    cells.iter().all(|cell| {
        let marker = cell.trim().trim_start_matches(':').trim_end_matches(':');
        marker.len() >= 3 && marker.chars().all(|character| character == '-')
    })
}

fn normalize_table_row(mut row: Vec<String>, columns: usize) -> Vec<String> {
    row.resize(columns, String::new());
    row.truncate(columns);
    row
}

fn render_table(
    header: &[String],
    rows: &[Vec<String>],
    width: usize,
    indent: &str,
) -> Vec<Line<'static>> {
    let columns = header.len();
    let separator_width = 3 * columns.saturating_sub(1);
    let available = width
        .saturating_sub(indent.chars().count())
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
                        .map(|(text, _)| text.chars().count())
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

fn render_table_row(
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
            let content_width: usize = cell_line
                .iter()
                .map(|span| span.content.chars().count())
                .sum();
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
fn inline_spans(text: &str) -> Vec<(String, Style)> {
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
fn wrap_styled(segments: &[(String, Style)], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_len = 0usize;
    let mut pending_space = false;
    for (text, style) in segments {
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

fn push_wrapped_word(
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

/// Truncate to `max` chars, appending an ellipsis when cut. Multi-line
/// input is flattened to its first line first.
pub fn truncate_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim_end();
    if line.chars().count() <= max {
        return line.to_string();
    }
    let mut out: String = line.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The transcript line announcing a tool call, e.g. `shell $ cargo test`
/// or `read_file src/main.rs`.
pub fn tool_call_line(name: &str, args: &Value) -> String {
    let detail = match name {
        "shell" => args
            .get("command")
            .and_then(Value::as_str)
            .map(|c| format!("$ {c}")),
        "read_file" | "write_file" | "edit_file" | "list_dir" => {
            args.get("path").and_then(Value::as_str).map(str::to_string)
        }
        "grep" => args
            .get("query")
            .and_then(Value::as_str)
            .map(|q| format!("'{q}'")),
        "process" => process_call_detail(args),
        _ => None,
    };
    let detail = detail.unwrap_or_else(|| args.to_string());
    truncate_line(&format!("{name} {detail}"), 100)
}

fn process_call_detail(args: &Value) -> Option<String> {
    let action = args.get("action")?.as_str()?;
    let id = args.get("id").and_then(Value::as_str);
    Some(match action {
        "spawn" => {
            let command = args.get("command").and_then(Value::as_str).unwrap_or("?");
            format!("spawn $ {command}")
        }
        "poll" => {
            let wait = args
                .get("waitMs")
                .and_then(Value::as_u64)
                .map(|ms| {
                    if ms >= 1_000 && ms % 1_000 == 0 {
                        format!(" · wait {}s", ms / 1_000)
                    } else {
                        format!(" · wait {ms}ms")
                    }
                })
                .unwrap_or_default();
            format!("poll {}{wait}", id.unwrap_or("?"))
        }
        "write" => {
            let input = args.get("input").and_then(Value::as_str).unwrap_or("");
            format!("write {} “{}”", id.unwrap_or("?"), truncate_line(input, 40))
        }
        "kill" => format!("kill {}", id.unwrap_or("?")),
        "list" => "list".to_string(),
        other => other.to_string(),
    })
}

/// The compact outcome line for a finished tool call.
pub fn tool_result_summary(name: &str, output: &Value, is_error: bool) -> String {
    if is_error {
        if name == "shell" {
            if let Some(exit) = output.get("exitCode").and_then(Value::as_i64) {
                return format!("exit {exit}");
            }
        }
        let message = output
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| output.as_str().map(str::to_string))
            .unwrap_or_else(|| output.to_string());
        return truncate_line(&format!("error: {message}"), 120);
    }
    let summary = match name {
        "shell" => {
            let exit = output
                .get("exitCode")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let stdout = output.get("stdout").and_then(Value::as_str).unwrap_or("");
            let stderr = output.get("stderr").and_then(Value::as_str).unwrap_or("");
            let head = [stdout, stderr]
                .iter()
                .flat_map(|s| s.lines())
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            Some(if head.is_empty() {
                format!("exit {exit}")
            } else {
                format!("exit {exit} · {head}")
            })
        }
        "read_file" => output
            .get("bytes")
            .and_then(Value::as_u64)
            .map(|b| format!("read {b} bytes")),
        "write_file" => output
            .get("bytesWritten")
            .and_then(Value::as_u64)
            .map(|b| format!("wrote {b} bytes")),
        "edit_file" => output
            .get("replacements")
            .and_then(Value::as_u64)
            .map(|n| format!("{n} replacement(s)")),
        "list_dir" => output
            .get("entries")
            .and_then(Value::as_array)
            .map(|e| format!("{} entries", e.len())),
        "process" => process_result_summary(output),
        _ => None,
    };
    let summary = summary.unwrap_or_else(|| output.to_string());
    truncate_line(&summary, 120)
}

fn process_result_summary(output: &Value) -> Option<String> {
    if let Some(processes) = output.get("processes").and_then(Value::as_array) {
        return Some(match processes.len() {
            0 => "no processes".to_string(),
            1 => "1 process".to_string(),
            count => format!("{count} processes"),
        });
    }

    let id = output.get("id").and_then(Value::as_str)?;
    let state = if output.get("running").and_then(Value::as_bool) == Some(true) {
        "running".to_string()
    } else if let Some(code) = output.get("exitCode").and_then(Value::as_i64) {
        format!("exit {code}")
    } else {
        "stopped".to_string()
    };
    let output_head = output
        .get("output")
        .and_then(Value::as_str)
        .and_then(|text| text.lines().find(|line| !line.trim().is_empty()))
        .map(str::trim);

    Some(match output_head {
        Some(head) => format!("{id} · {state} · {head}"),
        None => format!("{id} · {state}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_view_theme_uses_the_colored_palette() {
        // Palettes are pure; the active theme is process-global and can be
        // switched by the TUI, so assert on the palette itself rather than
        // the global (tests run in parallel and may switch it).
        let t = theme_for(ThemeName::Default);
        assert_eq!(t.accent.fg, Some(Color::Cyan));
        assert_eq!(t.code.fg, Some(Color::Cyan));
    }

    #[test]
    fn truncate_flattens_and_cuts_with_ellipsis() {
        assert_eq!(truncate_line("hello", 10), "hello");
        assert_eq!(truncate_line("hello world", 8), "hello w…");
        assert_eq!(truncate_line("line one\nline two", 20), "line one");
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
    fn grep_shows_the_query() {
        let line = tool_call_line("grep", &json!({"query": "fn main", "path": "."}));
        assert_eq!(line, "grep 'fn main'");
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
    fn unknown_tools_fall_back_to_compact_json() {
        let line = tool_call_line("deploy", &json!({"env": "prod"}));
        assert_eq!(line, "deploy {\"env\":\"prod\"}");
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
            "2 replacement(s)"
        );
        let out = json!({"path": ".", "entries": ["a", "b"]});
        assert_eq!(tool_result_summary("list_dir", &out, false), "2 entries");
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
        assert_eq!(code.style.fg, Some(Color::Cyan));
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
        assert!(texts.contains(&"  Title".to_string()), "{texts:?}");
        assert!(texts.contains(&"  • item one".to_string()), "{texts:?}");
        assert!(texts.contains(&"  • item two".to_string()), "{texts:?}");
        let title = lines.iter().find(|l| flat(l).contains("Title")).unwrap();
        assert!(title.spans[1].style.add_modifier.contains(Modifier::BOLD));
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
                .find(|span| {
                    span.content.as_ref() == code_word && span.style.fg == Some(Color::Cyan)
                })
                .unwrap_or_else(|| panic!("inline code word {code_word}"));
            assert_eq!(code.style.fg, Some(Color::Cyan));
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
