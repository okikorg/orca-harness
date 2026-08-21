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

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "help",
        description: "show available slash commands and keys",
        category: "General",
        takes_args: false,
    },
    CommandSpec {
        name: "clear",
        description: "reset the conversation context",
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
        name: "provider",
        description: "switch provider (openrouter, openai, local)",
        category: "General",
        takes_args: false,
    },
    CommandSpec {
        name: "subagents",
        description: "show or set subagent nesting depth (1-5)",
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
        name: "usage",
        description: "session token totals, cache traffic, and context occupancy",
        category: "Session",
        takes_args: false,
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
        description: "list recorded sessions for this workspace, or resume one by id",
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
        // "m" prefixes "models" and appears inside "compact" and "theme".
        assert_eq!(
            names(&filter_commands("m")),
            vec!["models", "compact", "theme"]
        );
        // "e" prefixes "expand" and "extensions"; appears inside every
        // other command except "quit"... which it doesn't contain.
        assert_eq!(
            names(&filter_commands("e")),
            vec![
                "expand",
                "extensions",
                "help",
                "clear",
                "queue",
                "models",
                "provider",
                "subagents",
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
}
