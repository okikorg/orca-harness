use super::{theme, Theme};
use crate::view::formatting::{sanitize_cells, truncate_line, truncate_styled_line};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::sync::OnceLock;
use syntect::{
    easy::HighlightLines,
    highlighting::{Theme as SyntaxTheme, ThemeSet as SyntaxThemeSet},
    parsing::SyntaxSet,
};
use syntect_tui::into_span;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static SYNTAX_THEMES: OnceLock<SyntaxThemeSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn syntax_theme() -> &'static SyntaxTheme {
    SYNTAX_THEMES
        .get_or_init(SyntaxThemeSet::load_defaults)
        .themes
        .get("base16-ocean.dark")
        .expect("syntect's default themes include base16-ocean.dark")
}

pub(super) fn render_code_block(
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
        let text = sanitize_cells(text);
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
