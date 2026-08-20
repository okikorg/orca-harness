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
        name: "expand",
        description: "full output of the n-th latest tool call (1 = latest)",
        category: "Tools",
        takes_args: true,
    },
    CommandSpec {
        name: "model",
        description: "show the current model, or switch with /model <id>",
        category: "General",
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
        name: "quit",
        description: "exit orca",
        category: "General",
        takes_args: false,
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
    }

    #[test]
    fn prefix_matches_rank_before_substring_matches() {
        // "el" prefixes nothing but appears inside "help", "model", "models".
        assert_eq!(
            names(&filter_commands("el")),
            vec!["help", "model", "models"]
        );
        // "m" prefixes "model" and "models" and appears nowhere else.
        assert_eq!(names(&filter_commands("m")), vec!["model", "models"]);
        // "e" prefixes "expand"; appears inside every other command except
        // "quit"... which it doesn't contain.
        assert_eq!(
            names(&filter_commands("e")),
            vec!["expand", "help", "clear", "model", "models", "provider"]
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
