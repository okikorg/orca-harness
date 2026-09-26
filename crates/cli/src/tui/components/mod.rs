//! Reusable TUI building blocks shared by the overlay trays.

pub mod activity_rail;
pub mod approval;
pub mod ask;
pub mod composer;
pub mod inspector;
pub mod keys;
mod layout;
pub mod message;
pub mod notification;
pub mod picker;
pub mod progress_list;
pub mod section;
pub mod status_bar;
pub mod subagent_row;
pub mod tabs;
pub(crate) mod tool_error;
pub mod tool_row;
pub mod transcript;
pub mod tree;
pub mod welcome;

/// Fit a line to `width` cells, for renderers outside this module.
pub(crate) fn layout_fit(
    line: ratatui::text::Line<'static>,
    width: usize,
) -> ratatui::text::Line<'static> {
    layout::fit(line, width)
}

#[cfg(test)]
mod render_tests;
