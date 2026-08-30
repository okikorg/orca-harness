use ratatui::text::Line;

use crate::tui::components::picker::ListPicker;

use super::matching_indices;
use crate::tui::PICKER_ROWS;

/// Agent Plugin registrations: saved enablement beside what this process
/// actually loaded. A mismatch is expected after a TUI management action and
/// explicitly says restart rather than implying the agent changed in place.
pub(crate) fn plugin_picker_lines(
    entries: &[crate::config::RegisteredPlugin],
    mcp: &crate::mcp::McpServers,
    skills: &crate::skills::Skills,
    filter: &str,
    picker: &ListPicker,
    width: usize,
) -> Vec<Line<'static>> {
    let indices = matching_indices(entries, filter, |entry| &entry.name);
    let rows = indices.into_iter().map(|index| {
        let entry = &entries[index];
        let saved = if entry.enabled { "on " } else { "off" };
        let (live, detail) = plugin_runtime_cells(
            entry,
            mcp.plugin_state(&entry.name),
            skills.plugin_count(&entry.name),
            mcp.plugin_hook_count(&entry.name),
        );
        [
            entry.name.clone(),
            saved.to_string(),
            live,
            entry.root.display().to_string(),
            detail,
        ]
    });
    let filter_note = if filter.is_empty() {
        "type to filter".into()
    } else {
        format!("filter: {filter}")
    };
    picker.windowed_table_lines(
        &format!("Plugins · saved/live · {filter_note} · ↑↓ move · enter toggle · esc close"),
        rows,
        [(4, 20), (3, 3), (12, 28), (0, 30), (0, usize::MAX)],
        width,
        PICKER_ROWS,
    )
}

fn plugin_runtime_cells(
    entry: &crate::config::RegisteredPlugin,
    state: crate::mcp::PluginRuntimeState,
    skills: usize,
    hooks: usize,
) -> (String, String) {
    use crate::mcp::PluginRuntimeState::*;
    let next = if entry.enabled {
        ""
    } else {
        " · off next run"
    };
    match state {
        NotLoaded if entry.enabled => ("restart to load".into(), String::new()),
        NotLoaded => ("not loaded".into(), String::new()),
        Starting if skills == 0 && hooks == 0 => ("starting".into(), String::new()),
        Starting => (
            format!(
                "live · {} · MCP starting",
                component_label(0, skills, hooks)
            ),
            String::new(),
        ),
        Loaded { servers, tools } => (
            loaded_label(tools, skills, hooks, next),
            if servers == 0 {
                "no MCP servers".into()
            } else {
                format!("{servers} server{} connected", plural(servers))
            },
        ),
        Degraded {
            connected,
            servers,
            tools,
            warning,
        } => (
            format!(
                "degraded · {}{}",
                component_label(tools, skills, hooks),
                next
            ),
            format!("{connected}/{servers} servers · {warning}"),
        ),
        Failed {
            connected,
            servers,
            tools,
            error,
        } if skills == 0 && hooks == 0 => (
            format!("failed {connected}/{servers}{next}"),
            format!("{tools} tool{} · {error}", plural(tools)),
        ),
        Failed { error, .. } => (
            format!("degraded · {}{}", component_label(0, skills, hooks), next),
            format!("MCP failed · {error}"),
        ),
    }
}

fn loaded_label(tools: usize, skills: usize, hooks: usize, next: &str) -> String {
    if tools == 0 && skills == 0 && hooks == 0 {
        format!("loaded{next}")
    } else {
        format!("live · {}{next}", component_label(tools, skills, hooks))
    }
}

fn component_label(tools: usize, skills: usize, hooks: usize) -> String {
    let mut parts = Vec::new();
    if tools > 0 {
        parts.push(format!("{tools} tool{}", plural(tools)));
    }
    if skills > 0 {
        parts.push(skill_label(skills));
    }
    if hooks > 0 {
        parts.push(format!("{hooks} hook{}", plural(hooks)));
    }
    if parts.is_empty() {
        "no live components".into()
    } else {
        parts.join(" · ")
    }
}

fn skill_label(skills: usize) -> String {
    format!("{skills} skill{}", plural(skills))
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> crate::config::RegisteredPlugin {
        crate::config::RegisteredPlugin {
            name: "useful-plugin".into(),
            root: "/tmp/useful-plugin".into(),
            enabled: true,
        }
    }

    #[test]
    fn live_label_combines_skill_and_mcp_components() {
        let (live, detail) = plugin_runtime_cells(
            &entry(),
            crate::mcp::PluginRuntimeState::Loaded {
                servers: 1,
                tools: 2,
            },
            1,
            1,
        );

        assert_eq!(live, "live · 2 tools · 1 skill · 1 hook");
        assert_eq!(detail, "1 server connected");
    }

    #[test]
    fn healthy_skill_keeps_failed_mcp_plugin_degraded_not_wholly_failed() {
        let (live, detail) = plugin_runtime_cells(
            &entry(),
            crate::mcp::PluginRuntimeState::Failed {
                connected: 0,
                servers: 1,
                tools: 0,
                error: "server closed the connection".into(),
            },
            1,
            0,
        );

        assert_eq!(live, "degraded · 1 skill");
        assert!(detail.contains("MCP failed"));
    }
}
