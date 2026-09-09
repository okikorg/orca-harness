//! Reuse the live answer on input, scrolling, and animation-only frames.

use std::sync::Arc;

use ratatui::text::Line;

use crate::view::{self, glyphs::UiStyle, ThemeName};

#[derive(Default)]
pub(crate) struct StreamingMarkdownCache {
    entry: Option<Entry>,
}

struct Entry {
    source: String,
    width: usize,
    theme: ThemeName,
    style: UiStyle,
    lines: Arc<Vec<Line<'static>>>,
}

impl StreamingMarkdownCache {
    pub(crate) fn lines(&mut self, source: &str, width: usize) -> Arc<Vec<Line<'static>>> {
        let theme = view::theme_name();
        let style = view::glyphs::ui_style();
        if let Some(entry) = &self.entry {
            if entry.source == source
                && entry.width == width
                && entry.theme == theme
                && entry.style == style
            {
                return Arc::clone(&entry.lines);
            }
        }
        let lines = Arc::new(view::markdown_lines(source, width, "  "));
        self.entry = Some(Entry {
            source: source.to_owned(),
            width,
            theme,
            style,
            lines: Arc::clone(&lines),
        });
        lines
    }

    pub(crate) fn clear(&mut self) {
        self.entry = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_answer_reuses_rendering_but_updates_and_resize_do_not() {
        let _guard = crate::tui::THEME_GUARD.lock().unwrap();
        let mut cache = StreamingMarkdownCache::default();
        let first = cache.lines("some **streaming** text", 80);
        assert!(Arc::ptr_eq(
            &first,
            &cache.lines("some **streaming** text", 80)
        ));
        let changed = cache.lines("some **streaming** code", 80);
        assert!(!Arc::ptr_eq(&first, &changed));
        assert_eq!(
            *changed,
            view::markdown_lines("some **streaming** code", 80, "  ")
        );
        let resized = cache.lines("some **streaming** code", 12);
        assert!(!Arc::ptr_eq(&changed, &resized));
        assert_eq!(
            *resized,
            view::markdown_lines("some **streaming** code", 12, "  ")
        );
        cache.clear();
        assert!(!Arc::ptr_eq(
            &resized,
            &cache.lines("some **streaming** code", 12)
        ));
    }

    #[test]
    fn streaming_fences_and_tables_match_uncached_rendering_at_every_character() {
        let _guard = crate::tui::THEME_GUARD.lock().unwrap();
        let sources = [
            "# Result\n```rust\nlet greeting = \"héllo\";\n```\nFinished.",
            "| Name | Value |\n| --- | --- |\n| α | 42 |\n\nAfter the table.",
            "```mermaid\nflowchart TD\nA[Start] --> B[End]\n```",
        ];
        let mut cache = StreamingMarkdownCache::default();
        for source in sources {
            for end in source.char_indices().map(|(i, _)| i).chain([source.len()]) {
                let partial = &source[..end];
                assert_eq!(
                    *cache.lines(partial, 80),
                    view::markdown_lines(partial, 80, "  ")
                );
            }
        }
    }

    #[test]
    fn theme_and_glyph_changes_invalidate_the_answer() {
        let _guard = crate::tui::THEME_GUARD.lock().unwrap();
        let original_theme = view::theme_name();
        let original_style = view::glyphs::ui_style();
        let mut cache = StreamingMarkdownCache::default();
        view::set_theme(ThemeName::Orca);
        view::glyphs::set_ui_style(UiStyle::Minimal);
        let first = cache.lines("# Heading\n`code`", 80);
        view::set_theme(ThemeName::Mono);
        let themed = cache.lines("# Heading\n`code`", 80);
        assert!(!Arc::ptr_eq(&first, &themed));
        view::glyphs::set_ui_style(UiStyle::Glyph);
        let styled = cache.lines("# Heading\n`code`", 80);
        assert!(!Arc::ptr_eq(&themed, &styled));
        assert_eq!(*styled, view::markdown_lines("# Heading\n`code`", 80, "  "));
        view::set_theme(original_theme);
        view::glyphs::set_ui_style(original_style);
    }
}
