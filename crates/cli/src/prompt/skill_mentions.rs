//! Model-facing expansion for explicit `$skill-name` invocations.

use std::collections::HashSet;

use crate::skills::SkillEntry;

/// Preserve the user's prompt while making selected skill mentions an
/// unambiguous instruction to call the dynamic `skill` tool. Only exact,
/// whitespace-delimited names from the current invokable catalog count;
/// currency, shell variables, and unknown `$tokens` stay ordinary text.
pub(crate) fn expand_skill_mentions(prompt: &str, skills: &[SkillEntry]) -> String {
    let known: HashSet<&str> = skills.iter().map(|entry| entry.name.as_str()).collect();
    let mut invoked = Vec::new();
    for token in prompt.split_whitespace() {
        let Some(name) = token.strip_prefix('$') else {
            continue;
        };
        if known.contains(name) && !invoked.contains(&name) {
            invoked.push(name);
        }
    }
    if invoked.is_empty() {
        return prompt.to_string();
    }

    format!(
        "The user explicitly invoked these skills: {}. Before any other work, call the `skill` \
         tool for each exact name, read all of its instructions, and follow them for this request.\n\n\
         {prompt}",
        invoked.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::skills::SkillState;

    fn entry(name: &str) -> SkillEntry {
        SkillEntry {
            name: name.into(),
            description: String::new(),
            enabled: true,
            state: SkillState::Loaded {
                root: ".orca/skills".into(),
                bytes: 1,
            },
            dir: PathBuf::new(),
            removable: false,
        }
    }

    #[test]
    fn expands_exact_known_mentions_once_in_prompt_order() {
        let skills = [entry("review"), entry("testing")];
        let expanded = expand_skill_mentions("$review test this with $testing $review", &skills);

        assert!(expanded.starts_with(
            "The user explicitly invoked these skills: review, testing. Before any other work"
        ));
        assert!(expanded.ends_with("$review test this with $testing $review"));
    }

    #[test]
    fn leaves_unknown_and_non_token_dollars_unchanged() {
        let skills = [entry("review")];
        for prompt in ["costs $5", "use $unknown", "prefix$review", "use $review,"] {
            assert_eq!(expand_skill_mentions(prompt, &skills), prompt);
        }
    }
}
