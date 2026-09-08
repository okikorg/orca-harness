//! Reusable TUI building blocks shared by the overlay trays.

pub mod activity_rail;
pub mod approval;
pub mod ask;
pub mod composer;
pub mod inspector;
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

#[cfg(test)]
mod render_tests;
