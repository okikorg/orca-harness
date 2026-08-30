//! One compact startup inventory plus separate actionable diagnostics.

use crate::mcp::view::StartupInventory;

pub(crate) fn notices(
    mcp: &crate::mcp::McpServers,
    skills: &crate::skills::Skills,
    hooks: usize,
    mcp_notices: Vec<String>,
    skill_notices: &[String],
) -> Vec<String> {
    consolidate(
        mcp.startup_inventory(),
        skills.loaded_count(),
        hooks,
        mcp_notices,
        skill_notices,
    )
}

fn consolidate(
    inventory: StartupInventory,
    skills: usize,
    hooks: usize,
    mcp_notices: Vec<String>,
    skill_notices: &[String],
) -> Vec<String> {
    let mut lines = Vec::new();
    if inventory.plugins + inventory.servers + inventory.tools + skills + hooks > 0 {
        lines.push(format!(
            "loaded · {} · {} · {} · {} · {}",
            count(inventory.plugins, "plugin", "plugins"),
            count(inventory.servers, "MCP server", "MCP servers"),
            count(inventory.tools, "MCP tool", "MCP tools"),
            count(skills, "skill", "skills"),
            count(hooks, "hook", "hooks"),
        ));
    }
    lines.extend(
        mcp_notices
            .into_iter()
            .filter(|line| !inventory.connected_notices.contains(line)),
    );
    lines.extend(
        skill_notices
            .iter()
            .filter(|line| !is_skill_count(line))
            .cloned(),
    );
    lines
}

fn count(value: usize, singular: &str, plural: &str) -> String {
    format!("{value} {}", if value == 1 { singular } else { plural })
}

fn is_skill_count(line: &str) -> bool {
    line == "skills · none found"
        || line
            .strip_prefix("skills · ")
            .and_then(|line| line.strip_suffix(" loaded"))
            .is_some_and(|count| count.parse::<usize>().is_ok())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn successes_collapse_into_one_inventory_and_diagnostics_remain() {
        let inventory = StartupInventory {
            plugins: 3,
            servers: 3,
            tools: 6,
            connected_notices: BTreeSet::from([
                "Plugin explore MCP files connected · 4 tools".into(),
                "MCP docs connected · 2 tools".into(),
            ]),
        };
        let lines = consolidate(
            inventory,
            18,
            4,
            vec![
                "Plugin explore MCP files connected · 4 tools".into(),
                "MCP docs connected · 2 tools".into(),
                "Plugin broken MCP tools · handshake failed".into(),
            ],
            &[
                "skills · 18 loaded".into(),
                "skill invalid (plugin:broken) · missing description".into(),
            ],
        );

        assert_eq!(
            lines,
            [
                "loaded · 3 plugins · 3 MCP servers · 6 MCP tools · 18 skills · 4 hooks",
                "Plugin broken MCP tools · handshake failed",
                "skill invalid (plugin:broken) · missing description",
            ]
        );
    }

    #[test]
    fn empty_healthy_inventory_does_not_add_noise() {
        let inventory = StartupInventory {
            plugins: 0,
            servers: 0,
            tools: 0,
            connected_notices: BTreeSet::new(),
        };
        assert_eq!(
            consolidate(inventory, 0, 0, vec!["Plugin broken · invalid".into()], &[]),
            ["Plugin broken · invalid"]
        );
    }
}
