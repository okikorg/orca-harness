//! The slash-command registry and the palette's filter logic. Pure data
//! and functions — the TUI renders whatever this returns.

/// One entry in the command palette.
pub struct CommandSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    /// True when the command accepts arguments (shown as a hint).
    pub takes_args: bool,
}

/// One user-facing keyboard shortcut. Keeping the labels in one catalog
/// gives `/hotkeys` a single inventory to render as bindings evolve.
pub struct HotkeySpec {
    pub keys: &'static str,
    pub description: &'static str,
    pub category: &'static str,
}

pub const HOTKEYS: &[HotkeySpec] = &[
    HotkeySpec {
        keys: "Enter",
        description: "send a prompt, or queue it while the agent is running",
        category: "Composer",
    },
    HotkeySpec {
        keys: "Tab",
        description: "complete the selected slash command",
        category: "Composer",
    },
    HotkeySpec {
        keys: "Shift+Tab",
        description: "cycle normal, plan, auto, and yolo modes",
        category: "Composer",
    },
    HotkeySpec {
        keys: "Ctrl+A / Ctrl+E",
        description: "move to the start / end of the prompt",
        category: "Composer",
    },
    HotkeySpec {
        keys: "Ctrl+U",
        description: "clear the prompt",
        category: "Composer",
    },
    HotkeySpec {
        keys: "Ctrl+V",
        description: "paste a clipboard image",
        category: "Composer",
    },
    HotkeySpec {
        keys: "Backspace / Delete",
        description: "delete before / after the prompt cursor",
        category: "Composer",
    },
    HotkeySpec {
        keys: "Left / Right",
        description: "move the prompt cursor",
        category: "Composer",
    },
    HotkeySpec {
        keys: "Home / End",
        description: "move to the start / end of the prompt",
        category: "Composer",
    },
    HotkeySpec {
        keys: "Up / Down",
        description: "browse prompt history, or move in the command palette",
        category: "Navigation",
    },
    HotkeySpec {
        keys: "Shift+Up / Down",
        description: "scroll the transcript one line",
        category: "Navigation",
    },
    HotkeySpec {
        keys: "PgUp / PgDn",
        description: "scroll the transcript or page through the active list",
        category: "Navigation",
    },
    HotkeySpec {
        keys: "Ctrl+O",
        description: "expand the latest work or tool output",
        category: "Actions",
    },
    HotkeySpec {
        keys: "Ctrl+Y",
        description: "copy the last answer",
        category: "Actions",
    },
    HotkeySpec {
        keys: "Esc",
        description: "close the active UI, clear the prompt, or interrupt a run",
        category: "Actions",
    },
    HotkeySpec {
        keys: "Ctrl+C",
        description: "interrupt, clear the prompt, or quit when idle",
        category: "Actions",
    },
    HotkeySpec {
        keys: "Ctrl+D",
        description: "quit when the prompt is empty",
        category: "Actions",
    },
    HotkeySpec {
        keys: "Enter / Right / Tab",
        description: "use the selected picker item (Tab in mention pickers)",
        category: "Pickers",
    },
    HotkeySpec {
        keys: "Left",
        description: "return to the previous picker",
        category: "Pickers",
    },
    HotkeySpec {
        keys: "Space",
        description: "toggle where supported, or reveal the selected row's actions",
        category: "Pickers",
    },
    HotkeySpec {
        keys: "Type / Backspace",
        description: "narrow / widen a filterable picker",
        category: "Pickers",
    },
    HotkeySpec {
        keys: "d / t",
        description: "delete / toggle when shown in a picker's action strip",
        category: "Pickers",
    },
    HotkeySpec {
        keys: "Esc / Ctrl+C",
        description: "close or cancel the active overlay",
        category: "Pickers",
    },
    HotkeySpec {
        keys: "Enter / q",
        description: "close the session usage overlay",
        category: "Usage",
    },
    HotkeySpec {
        keys: "Type / Backspace / Enter",
        description: "edit / erase / confirm an API key",
        category: "API key",
    },
    HotkeySpec {
        keys: "Tab / Shift+Tab",
        description: "move to the next / previous clarification topic",
        category: "Ask form",
    },
    HotkeySpec {
        keys: "Up / Down",
        description: "move between clarification questions",
        category: "Ask form",
    },
    HotkeySpec {
        keys: "Left / Right",
        description: "move between clarification options",
        category: "Ask form",
    },
    HotkeySpec {
        keys: "Space / Enter",
        description: "choose an option / submit clarification",
        category: "Ask form",
    },
    HotkeySpec {
        keys: "Type / Backspace",
        description: "edit the focused clarification answer",
        category: "Ask form",
    },
    HotkeySpec {
        keys: "Esc / Ctrl+C",
        description: "cancel clarification",
        category: "Ask form",
    },
    HotkeySpec {
        keys: "y / Y",
        description: "approve a tool call once",
        category: "Approval",
    },
    HotkeySpec {
        keys: "a / A",
        description: "approve for the session / save for this workspace",
        category: "Approval",
    },
    HotkeySpec {
        keys: "n / N / Esc / Ctrl+C",
        description: "deny a tool call",
        category: "Approval",
    },
];

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "help",
        description: "show available slash commands",
        category: "General",
        takes_args: false,
    },
    CommandSpec {
        name: "hotkeys",
        description: "show all registered keyboard shortcuts",
        category: "General",
        takes_args: false,
    },
    CommandSpec {
        name: "clear",
        description: "preserve this session, start fresh, and stop background work",
        category: "Session",
        takes_args: false,
    },
    CommandSpec {
        name: "compact",
        description: "compact the conversation: elide tool outputs, keep a capped summary",
        category: "Session",
        takes_args: false,
    },
    CommandSpec {
        name: "rewind",
        description: "drop the last n user turns from the conversation (default 1)",
        category: "Session",
        takes_args: true,
    },
    CommandSpec {
        name: "fork",
        description:
            "continue this conversation in a new session, leaving the current one as it is",
        category: "Session",
        takes_args: false,
    },
    CommandSpec {
        name: "refine",
        description: "propose one skill from this session; approve with y/n, revert with undo",
        category: "Session",
        takes_args: true,
    },
    CommandSpec {
        name: "queue",
        description: "show the prompt queue, or clear waiting prompts",
        category: "Session",
        takes_args: true,
    },
    CommandSpec {
        name: "expand",
        description: "full output of the n-th latest tool call (1 = latest)",
        category: "Tools",
        takes_args: true,
    },
    CommandSpec {
        name: "models",
        description: "pick a model from the endpoint's catalog",
        category: "General",
        takes_args: true,
    },
    CommandSpec {
        name: "effort",
        description: "change reasoning effort for the active model",
        category: "General",
        takes_args: false,
    },
    CommandSpec {
        name: "provider",
        description: "switch provider (openrouter, openai, local)",
        category: "General",
        takes_args: false,
    },
    CommandSpec {
        name: "subagents",
        description: "configure subagent model, depth, steps, timeout, output, and retries",
        category: "Session",
        takes_args: true,
    },
    CommandSpec {
        name: "extensions",
        description: "toggle harness extensions (no argument opens the picker)",
        category: "Session",
        takes_args: true,
    },
    CommandSpec {
        name: "mcp",
        description: "manage standalone MCP and view plugin servers",
        category: "Session",
        takes_args: true,
    },
    CommandSpec {
        name: "plugin",
        description: "list and manage Agent Plugins; no argument opens the picker",
        category: "Session",
        takes_args: true,
    },
    CommandSpec {
        name: "skills",
        description: "list and toggle skills, add <source>, create <name>, remove <name>",
        category: "Session",
        takes_args: true,
    },
    CommandSpec {
        name: "mode",
        description: "pick a session mode (normal, plan, auto review, or yolo); no argument opens the picker",
        category: "Session",
        takes_args: true,
    },
    CommandSpec {
        name: "todo",
        description: "show the agent's current task list",
        category: "Session",
        takes_args: false,
    },
    CommandSpec {
        name: "usage",
        description: "session token totals, cache traffic, and context occupancy",
        category: "Session",
        takes_args: false,
    },
    CommandSpec {
        name: "copy",
        description: "copy the last answer to the clipboard — code for its last code block, all for the transcript, tool for the inspected tool",
        category: "General",
        takes_args: true,
    },
    CommandSpec {
        name: "quit",
        description: "exit orcacode",
        category: "General",
        takes_args: false,
    },
    CommandSpec {
        name: "theme",
        description: "pick a color theme (no argument opens the picker)",
        category: "General",
        takes_args: true,
    },
    CommandSpec {
        name: "settings",
        description: "view and change provider, model, theme, and api key",
        category: "General",
        takes_args: false,
    },
    CommandSpec {
        name: "sessions",
        description: "resume a recorded session (no argument opens the picker)",
        category: "Session",
        takes_args: true,
    },
];

/// Filter the registry against what the user typed after `/`. Only the
/// first whitespace-separated token filters (the rest is arguments).
/// Prefix matches rank before substring matches; both preserve registry
/// order among themselves.
pub fn filter_commands(query: &str) -> Vec<&'static CommandSpec> {
    let token = query.split_whitespace().next().unwrap_or("").to_lowercase();
    if token.is_empty() {
        return COMMANDS.iter().collect();
    }
    let mut prefix: Vec<&CommandSpec> = Vec::new();
    let mut substring: Vec<&CommandSpec> = Vec::new();
    for spec in COMMANDS {
        if spec.name.starts_with(&token) {
            prefix.push(spec);
        } else if spec.name.contains(&token) {
            substring.push(spec);
        }
    }
    prefix.extend(substring);
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(specs: &[&CommandSpec]) -> Vec<&'static str> {
        specs.iter().map(|s| s.name).collect()
    }

    #[test]
    fn empty_query_lists_every_command() {
        let all = filter_commands("");
        assert_eq!(all.len(), COMMANDS.len());
        assert_eq!(all[0].name, "help");
        assert!(all.iter().all(|spec| spec.name != "model"));
        assert!(all.iter().any(|spec| spec.name == "queue"));
    }

    #[test]
    fn prefix_matches_rank_before_substring_matches() {
        // "el" prefixes nothing but appears inside "help" and "models".
        assert_eq!(names(&filter_commands("el")), vec!["help", "models"]);
        // "m" prefixes "models", "mcp", and "mode"; appears
        // inside "compact" and "theme", and inside "settings".
        assert_eq!(
            names(&filter_commands("m")),
            vec!["models", "mcp", "mode", "compact", "theme"]
        );
        // "e" prefixes "expand" and "extensions"; the substring matches
        // that follow keep registry order among themselves.
        assert_eq!(
            names(&filter_commands("e")),
            vec![
                "expand",
                "effort",
                "extensions",
                "help",
                "hotkeys",
                "clear",
                "rewind",
                "refine",
                "queue",
                "models",
                "provider",
                "subagents",
                "mode",
                "usage",
                "theme",
                "settings",
                "sessions"
            ]
        );
    }

    #[test]
    fn arguments_do_not_affect_filtering() {
        assert_eq!(names(&filter_commands("expand 3")), vec!["expand"]);
    }

    #[test]
    fn unknown_query_matches_nothing() {
        assert!(filter_commands("zzz").is_empty());
    }

    #[test]
    fn plugin_management_is_in_the_command_palette() {
        let plugin = COMMANDS.iter().find(|command| command.name == "plugin");
        assert!(plugin.is_some(), "/plugin is missing from the palette");
        assert!(plugin.unwrap().takes_args);
    }
}
