//! Unified MCP inventory: mutable standalone rows and read-only plugin rows.

use ratatui::text::Line;

use super::components::picker::ListPicker;
use super::format::redact_command;
use super::PICKER_ROWS;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    Standalone(crate::config::McpServer),
    Plugin(crate::mcp::view::PluginMcpEntry),
}

impl Entry {
    pub fn label(&self) -> String {
        match self {
            Self::Standalone(server) => server.name.clone(),
            Self::Plugin(server) => server.label(),
        }
    }
}

pub(crate) fn snapshot(mcp: &crate::mcp::McpServers) -> Vec<Entry> {
    let mut entries = crate::config::stored_mcp_servers()
        .into_iter()
        .map(Entry::Standalone)
        .collect::<Vec<_>>();
    entries.extend(mcp.plugin_mcp_entries().into_iter().map(Entry::Plugin));
    entries
}

pub(crate) fn matching_indices(entries: &[Entry], filter: &str) -> Vec<usize> {
    let needle = filter.to_lowercase();
    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| needle.is_empty() || entry.label().to_lowercase().contains(&needle))
        .map(|(index, _)| index)
        .collect()
}

pub(crate) fn lines(
    entries: &[Entry],
    mcp: &crate::mcp::McpServers,
    filter: &str,
    picker: &ListPicker,
    width: usize,
) -> Vec<Line<'static>> {
    let rows = matching_indices(entries, filter)
        .into_iter()
        .map(|index| row(&entries[index], mcp));
    let filter_note = if filter.is_empty() {
        "type to filter".into()
    } else {
        format!("filter: {filter}")
    };
    picker.windowed_table_lines(
        &format!(
            "MCP servers · {filter_note} · enter/space toggle standalone · plugin rows read-only · esc close"
        ),
        rows,
        [(4, 24), (3, 6), (9, 9), (0, usize::MAX)],
        width,
        PICKER_ROWS,
    )
}

fn row(entry: &Entry, mcp: &crate::mcp::McpServers) -> [String; 4] {
    match entry {
        Entry::Standalone(server) => {
            let (count, why) = standalone_state(server, mcp);
            let state = if server.enabled { "on" } else { "off" };
            [
                server.name.clone(),
                state.into(),
                count,
                format!("{}{why}", redact_command(&server.command)),
            ]
        }
        Entry::Plugin(server) => {
            let (count, why) = state_cells(mcp.plugin_mcp_state(server));
            [
                server.label(),
                "plugin".into(),
                count,
                format!("read-only · manage with /plugin{why}"),
            ]
        }
    }
}

fn standalone_state(
    server: &crate::config::McpServer,
    mcp: &crate::mcp::McpServers,
) -> (String, String) {
    if server.enabled {
        state_cells(mcp.state(&server.name))
    } else {
        (String::new(), String::new())
    }
}

fn state_cells(state: Option<crate::mcp::McpState>) -> (String, String) {
    match state {
        Some(crate::mcp::McpState::Connected(1)) => ("1 tool".into(), String::new()),
        Some(crate::mcp::McpState::Connected(n)) => (format!("{n} tools"), String::new()),
        Some(crate::mcp::McpState::Failed(error)) => ("failed".into(), format!(" · {error}")),
        None => ("…".into(), String::new()),
    }
}
