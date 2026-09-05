use super::formatting::*;

// Pure rendering helpers: turn tool calls and results into the compact
// one-liners the transcript shows, and assistant markdown into styled
// lines. No terminal state — unit-testable.

use std::sync::RwLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
mod code;

pub use code::highlighted_code_lines;
use code::render_code_block;

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
    Orca,
}

impl ThemeName {
    pub const ALL: [ThemeName; 8] = [
        Self::Default,
        Self::Mono,
        Self::Dracula,
        Self::SolarizedDark,
        Self::OneDark,
        Self::Monokai,
        Self::Nord,
        Self::Orca,
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
            Self::Orca => "orca",
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
            "orca" => Some(Self::Orca),
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
            Self::Orca => "Orca",
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
    /// Emphasis: user prompts, section headers, the banner.
    pub strong: Style,
    /// The cursor row in a picker, palette, or list. A notch below
    /// [`Self::strong`]: a selection marks where you are, it does not
    /// compete with the prompts and headers around it.
    pub select: Style,
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
        select: Style::default().fg(Color::Gray),
        warn: bold,
        error: bold,
        success: Style::default(),
        code: Style::default().add_modifier(Modifier::ITALIC),
    }
}

/// The default theme's primary hue: a muted steel blue (#88A1BB) used
/// for accents and inline code.
const PRIMARY: Color = Color::Rgb(136, 161, 187);

/// Desaturated brick red (#BF6C69) for errors — loud enough to spot,
/// quiet enough to sit next to [`PRIMARY`].
const ERROR: Color = Color::Rgb(191, 108, 105);

pub fn default_theme() -> Theme {
    Theme {
        dim: Style::default().fg(Color::DarkGray),
        accent: Style::default().fg(PRIMARY),
        strong: Style::default().add_modifier(Modifier::BOLD),
        select: Style::default().fg(Color::Gray),
        warn: Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        error: Style::default().fg(ERROR),
        success: Style::default().fg(Color::Green),
        code: Style::default().fg(PRIMARY),
    }
}

/// Orca's signature palette: paper-white type, graphite chrome, and a single
/// mint signal for active work. It intentionally stays restrained like the
/// product surfaces shown in the Orca visual system.
pub fn orca_theme() -> Theme {
    const GRAPHITE: Color = Color::Rgb(127, 127, 127);
    const MINT: Color = Color::Rgb(89, 224, 154);
    const PAPER: Color = Color::Rgb(244, 244, 244);

    Theme {
        dim: Style::default().fg(GRAPHITE),
        accent: Style::default().fg(MINT),
        strong: Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
        select: Style::default().fg(PAPER),
        warn: Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
        error: Style::default().fg(Color::Rgb(224, 128, 128)),
        success: Style::default().fg(MINT),
        code: Style::default().fg(PAPER),
    }
}

// https://draculatheme.com
pub(super) fn dracula_theme() -> Theme {
    Theme {
        dim: Style::default().fg(Color::Rgb(98, 114, 164)), // comment
        accent: Style::default().fg(Color::Rgb(255, 121, 198)), // pink
        strong: Style::default()
            .fg(Color::Rgb(189, 147, 249))
            .add_modifier(Modifier::BOLD), // purple bold
        select: Style::default().fg(Color::Rgb(189, 147, 249)), // purple
        warn: Style::default()
            .fg(Color::Rgb(241, 250, 140))
            .add_modifier(Modifier::BOLD), // yellow bold
        error: Style::default().fg(Color::Rgb(255, 85, 85)), // red
        success: Style::default().fg(Color::Rgb(80, 250, 123)), // green
        code: Style::default().fg(Color::Rgb(139, 233, 253)), // cyan
    }
}

// https://ethanschoonover.com/solarized  (dark variant)
pub(super) fn solarized_dark_theme() -> Theme {
    Theme {
        dim: Style::default().fg(Color::Rgb(131, 148, 150)), // base0
        accent: Style::default().fg(Color::Rgb(38, 139, 210)), // blue
        strong: Style::default()
            .fg(Color::Rgb(147, 161, 161))
            .add_modifier(Modifier::BOLD), // base1 bold
        select: Style::default().fg(Color::Rgb(147, 161, 161)), // base1
        warn: Style::default()
            .fg(Color::Rgb(181, 137, 0))
            .add_modifier(Modifier::BOLD), // yellow bold
        error: Style::default().fg(Color::Rgb(220, 50, 47)), // red
        success: Style::default().fg(Color::Rgb(133, 153, 0)), // green
        code: Style::default().fg(Color::Rgb(42, 161, 152)), // cyan
    }
}

// Atom One Dark
pub(super) fn one_dark_theme() -> Theme {
    Theme {
        dim: Style::default().fg(Color::Rgb(92, 99, 112)), // comment
        accent: Style::default().fg(Color::Rgb(97, 175, 239)), // blue
        strong: Style::default()
            .fg(Color::Rgb(229, 192, 123))
            .add_modifier(Modifier::BOLD), // yellow bold
        select: Style::default().fg(Color::Rgb(229, 192, 123)), // yellow
        warn: Style::default()
            .fg(Color::Rgb(209, 154, 102))
            .add_modifier(Modifier::BOLD), // orange bold
        error: Style::default().fg(Color::Rgb(224, 108, 117)), // red
        success: Style::default().fg(Color::Rgb(152, 195, 121)), // green
        code: Style::default().fg(Color::Rgb(86, 182, 194)), // cyan
    }
}

// Monokai
pub(super) fn monokai_theme() -> Theme {
    Theme {
        dim: Style::default().fg(Color::Rgb(117, 113, 94)), // comment
        accent: Style::default().fg(Color::Rgb(166, 226, 46)), // green-bright
        strong: Style::default()
            .fg(Color::Rgb(249, 38, 114))
            .add_modifier(Modifier::BOLD), // pink bold
        select: Style::default().fg(Color::Rgb(249, 38, 114)), // pink
        warn: Style::default()
            .fg(Color::Rgb(230, 219, 116))
            .add_modifier(Modifier::BOLD), // yellow bold
        error: Style::default().fg(Color::Rgb(249, 38, 114)), // pink
        success: Style::default().fg(Color::Rgb(166, 226, 46)), // green
        code: Style::default().fg(Color::Rgb(102, 217, 239)), // cyan
    }
}

// Nord
pub(super) fn nord_theme() -> Theme {
    Theme {
        dim: Style::default().fg(Color::Rgb(76, 86, 106)), // nord3
        accent: Style::default().fg(Color::Rgb(136, 192, 208)), // nord8
        strong: Style::default()
            .fg(Color::Rgb(216, 222, 233))
            .add_modifier(Modifier::BOLD), // nord4 bold
        select: Style::default().fg(Color::Rgb(216, 222, 233)), // nord4
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
        ThemeName::Orca => orca_theme(),
    }
}

static THEME: RwLock<Option<ThemeName>> = RwLock::new(None);

/// Install the theme at startup. If already set, replaces it (allows
/// runtime switching via `/theme`).
pub(super) fn install_theme(theme: ThemeName) {
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
            let block = &raw_lines[code_start..code_end];
            let selected_theme = theme();
            let mermaid = language.eq_ignore_ascii_case("mermaid");
            let closed = code_end < raw_lines.len();
            let rendered = if mermaid && !closed {
                vec![super::mermaid::pending(indent, &selected_theme)]
            } else {
                mermaid
                    .then(|| super::mermaid::render(block, width, indent, &selected_theme))
                    .flatten()
                    .unwrap_or_else(|| {
                        render_code_block(block, language, width, indent, &selected_theme)
                    })
            };
            out.extend(rendered);
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
        if let Some(header) = trimmed.strip_prefix('#') {
            let level = 1 + header.chars().take_while(|c| *c == '#').count();
            let header = header.trim_start_matches('#').trim_start();
            // Bold alone vanishes on terminals with a weak bold face, so
            // the level also carries a visible prefix from the style table.
            let prefix = super::glyphs::glyphs().heading[level.min(3) - 1];
            out.push(Line::from(vec![
                Span::raw(indent.to_string()),
                Span::styled(prefix.to_string(), dim),
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

#[cfg(test)]
include!("tests.rs");
