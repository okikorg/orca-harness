//! Unified Skill inventory: mutable standalone rows and read-only plugin rows.

use ratatui::text::Line;

use super::components::picker::ListPicker;
use super::format::size;
use super::PICKER_ROWS;

pub(crate) fn plugin_name(entry: &crate::skills::SkillEntry) -> Option<&str> {
    let root = match &entry.state {
        crate::skills::SkillState::Loaded { root, .. }
        | crate::skills::SkillState::Shadowed { root, .. }
        | crate::skills::SkillState::Failed { root, .. } => root,
    };
    root.strip_prefix("plugin:")
}

pub(crate) fn lines(
    entries: &[crate::skills::SkillEntry],
    filter: &str,
    picker: &ListPicker,
    width: usize,
) -> Vec<Line<'static>> {
    let indices = super::render::matching_indices(entries, filter, |entry| &entry.name);
    let rows = indices.into_iter().map(|index| row(&entries[index]));
    let filter_note = if filter.is_empty() {
        "type to filter".into()
    } else {
        format!("filter: {filter}")
    };
    picker.windowed_table_lines(
        &format!("Skills · {filter_note} · enter toggle/use · plugin rows read-only · esc close"),
        rows,
        [(4, 20), (3, 6), (0, 28), (0, 8), (0, usize::MAX)],
        width,
        PICKER_ROWS,
    )
}

fn row(entry: &crate::skills::SkillEntry) -> [String; 5] {
    let plugin = plugin_name(entry);
    let (state, root, bytes, detail) = match &entry.state {
        crate::skills::SkillState::Loaded { root, bytes } => {
            let state = if plugin.is_some() {
                "plugin"
            } else if entry.enabled {
                "on"
            } else {
                "off"
            };
            let detail = if plugin.is_some() {
                format!(
                    "read-only · enter inserts ${} · {}",
                    entry.name, entry.description
                )
            } else {
                entry.description.clone()
            };
            (state, root.clone(), size(*bytes), detail)
        }
        crate::skills::SkillState::Shadowed { root, by } => (
            if plugin.is_some() { "plugin" } else { "—" },
            root.clone(),
            String::new(),
            format!("{}shadowed by {by}", read_only_prefix(plugin)),
        ),
        crate::skills::SkillState::Failed { root, reason } => (
            if plugin.is_some() { "plugin" } else { "—" },
            root.clone(),
            String::new(),
            format!("{}failed — {reason}", read_only_prefix(plugin)),
        ),
    };
    [entry.name.clone(), state.into(), root, bytes, detail]
}

fn read_only_prefix(plugin: Option<&str>) -> &'static str {
    if plugin.is_some() {
        "read-only · "
    } else {
        ""
    }
}
