//! Proposal validation. Every failure names the exact offending value
//! so a single repair retry can fix it; silent rejection taught us
//! nothing last time.

use super::proposal::SkillProposal;

pub const DESC_BYTE_LIMIT: usize = 256;

/// The Python surface packaged scripts may use. One visible artifact,
/// audited as a whole — never grown one rejected idiom at a time.
pub const PY_ALLOWED: &[&str] = &[
    "sys",
    "re",
    "os",
    "json",
    "pathlib",
    "collections",
    "itertools",
    "functools",
    "subprocess",
    "argparse",
    "textwrap",
    "difflib",
];

// Only names with real teeth. v1 died rejecting ordinary idioms
// (lambda, defaultdict, sys.exit) one at a time — a banned name must
// earn its place, not merely look dynamic.
const PY_BANNED_CALLABLES: &[&str] = &["exec", "eval", "__import__", "input", "compile"];

/// The command surface packaged shell scripts may call — read-and-report
/// text tooling only. Same rule as [`PY_ALLOWED`]: one visible artifact,
/// audited as a whole.
pub const SH_ALLOWED: &[&str] = &[
    "echo", "printf", "grep", "egrep", "rg", "sed", "awk", "cut", "sort", "uniq", "head", "tail",
    "wc", "tr", "find", "ls", "pwd", "stat", "du", "xargs", "cat", "test", "dirname", "basename",
    "git", "diff", "comm", "read", "jq", "date", "env", "which", "cargo", "make", "python3",
];

/// Shell words that structure a script rather than run a command.
const SH_KEYWORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "function", "local", "return", "exit", "break", "continue", "shift", "set", "[", "[[",
    "!", "{", "}", "(", ")",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub ok: bool,
    pub msg: String,
}

impl Check {
    fn new(ok: bool, msg: impl Into<String>) -> Self {
        Self {
            ok,
            msg: msg.into(),
        }
    }
}

pub fn validate(proposal: &SkillProposal, roster: &[String], existing: &[String]) -> Vec<Check> {
    let mut checks = Vec::new();

    let collision = existing.iter().any(|name| name == &proposal.name);
    checks.push(Check::new(
        !collision,
        match collision {
            true => format!(
                "name is unused (\"{}\" already names an installed skill — pick another)",
                proposal.name
            ),
            false => "name does not collide with an installed skill".into(),
        },
    ));

    let kebab = !proposal.name.is_empty()
        && proposal.name.split('-').all(|w| {
            !w.is_empty()
                && w.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        });
    checks.push(Check::new(
        kebab,
        format!("name is kebab-case (got \"{}\")", proposal.name),
    ));

    let bytes = proposal.description.len();
    checks.push(Check::new(
        bytes <= DESC_BYTE_LIMIT && bytes > 0,
        format!("description within 1..={DESC_BYTE_LIMIT} bytes (got {bytes})"),
    ));

    let unknown: Vec<&str> = proposal
        .citations
        .iter()
        .filter(|c| !roster.iter().any(|r| r == *c))
        .map(String::as_str)
        .collect();
    checks.push(Check::new(
        unknown.is_empty(),
        match unknown.is_empty() {
            true => "all citations resolve in the roster".into(),
            false => format!(
                "citations must be roster ids (unknown: {})",
                unknown.join(", ")
            ),
        },
    ));
    checks.push(Check::new(
        !proposal.citations.is_empty(),
        "at least one evidence citation",
    ));

    for script in &proposal.scripts {
        let python = script.path.ends_with(".py");
        let shell = script.path.ends_with(".sh");
        let path_ok =
            script.path.starts_with("scripts/") && (python || shell) && !script.path.contains("..");
        checks.push(Check::new(
            path_ok,
            format!(
                "script path is scripts/*.py or scripts/*.sh (got \"{}\")",
                script.path
            ),
        ));
        let (language, violations) = match shell {
            true => ("shell", shell_violations(&script.code)),
            false => ("Python", script_violations(&script.code)),
        };
        checks.push(Check::new(
            violations.is_empty(),
            match violations.is_empty() {
                true => format!("{} passes the {language} allowlist", script.path),
                false => format!(
                    "{} uses names outside the {language} allowlist: {}",
                    script.path,
                    violations.join(", ")
                ),
            },
        ));
    }

    checks
}

/// Imported modules and banned callables the allowlist rejects, in
/// order of first appearance, deduplicated.
fn script_violations(code: &str) -> Vec<String> {
    let mut violations: Vec<String> = Vec::new();
    let note = |name: &str, violations: &mut Vec<String>| {
        if !violations.iter().any(|v| v == name) {
            violations.push(name.to_string());
        }
    };
    // Comments execute nothing: a "# never use open here" must not fail
    // the scan. Lexical per-line stripping, same spirit as the rest.
    let code: String = code
        .lines()
        .map(|line| line.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    for line in code.lines() {
        let line = line.trim();
        let modules: Vec<&str> = match (line.strip_prefix("import "), line.strip_prefix("from ")) {
            (Some(rest), _) => rest.split(',').map(str::trim).collect(),
            (None, Some(rest)) => rest.split_whitespace().take(1).collect(),
            (None, None) => Vec::new(),
        };
        for module in modules {
            let root = module.split('.').next().unwrap_or(module);
            let root = root.split_whitespace().next().unwrap_or(root);
            if !root.is_empty() && !PY_ALLOWED.contains(&root) {
                note(root, &mut violations);
            }
        }
    }
    for banned in PY_BANNED_CALLABLES {
        let mut rest = code.as_str();
        while let Some(at) = rest.find(banned) {
            let before = rest[..at].chars().next_back();
            let after = rest[at + banned.len()..].chars().next();
            let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != '_');
            if boundary(before) && boundary(after) {
                note(banned, &mut violations);
                break;
            }
            rest = &rest[at + banned.len()..];
        }
    }
    violations
}

/// Commands the shell allowlist rejects: the first word of every
/// pipeline/list segment, minus keywords, assignments, and redirections.
/// Lexical like the Python scan — the point is a loud, named rejection,
/// not a parser.
fn shell_violations(code: &str) -> Vec<String> {
    let mut violations: Vec<String> = Vec::new();
    // Functions the script defines are its own to call: `run() { … }`
    // makes a later bare `run` an invocation, not a foreign command.
    let defined: Vec<&str> = code
        .lines()
        .filter_map(|line| {
            let line = line.trim().strip_prefix("function ").unwrap_or(line.trim());
            let name = line.split(['(', ' ']).next().unwrap_or("");
            let rest = &line[name.len()..];
            let is_definition = (rest.trim_start().starts_with("()") || rest.starts_with("()"))
                && !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            is_definition.then_some(name)
        })
        .collect();
    for line in code.lines() {
        let mut line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // A `case` arm pattern (`run|clippy|bench)`) is a matcher, not a
        // command list: drop the pattern token, keep scanning its body.
        if let Some(first) = line.split_whitespace().next() {
            if first.ends_with(')') && !first.starts_with('(') {
                line = line[first.len()..].trim_start();
            }
        }
        for segment in line.split(['|', ';']).flat_map(|s| s.split("&&")) {
            let mut words = segment.split_whitespace();
            let command = loop {
                match words.next() {
                    // Skip env-style prefixes (FOO=bar cmd …).
                    Some(word) if word.contains('=') && !word.starts_with('=') => continue,
                    other => break other,
                }
            };
            let Some(command) = command else { continue };
            let command = command.trim_start_matches(['$', '(']);
            // `name()` heads a function definition, not a call.
            let command = command.strip_suffix("()").unwrap_or(command);
            if command.is_empty()
                || SH_KEYWORDS.contains(&command)
                || SH_ALLOWED.contains(&command)
                || defined.contains(&command)
                || violations.iter().any(|v| v == command)
            {
                continue;
            }
            violations.push(command.to_string());
        }
    }
    violations
}

/// The message fed back to the proposer for its one repair retry.
pub fn repair_feedback(checks: &[Check]) -> String {
    let failures: Vec<&str> = checks
        .iter()
        .filter(|c| !c.ok)
        .map(|c| c.msg.as_str())
        .collect();
    format!(
        "Your proposal failed validation:\n- {}\nResubmit the corrected single JSON object \
         and nothing else.",
        failures.join("\n- ")
    )
}

#[cfg(test)]
mod tests {
    use super::super::proposal::{ScriptFile, SkillProposal};
    use super::*;

    fn roster() -> Vec<String> {
        vec!["e01".into(), "e02".into(), "e03".into()]
    }

    fn good_proposal() -> SkillProposal {
        SkillProposal {
            name: "retry-with-backoff".into(),
            description: "Use jittered exponential backoff for retries.".into(),
            body: "Always cap the delay.".into(),
            scripts: vec![ScriptFile {
                path: "scripts/check_backoff.py".into(),
                code: "import sys, re, pathlib\nsrc = pathlib.Path(sys.argv[1]).read_text()\n\
                       print([l for l in src.splitlines() if re.search(r\"sleep\\(0\", l)])"
                    .into(),
            }],
            citations: vec!["e02".into(), "e03".into()],
        }
    }

    #[test]
    fn a_clean_proposal_passes_every_check() {
        let checks = validate(&good_proposal(), &roster(), &[]);
        assert!(checks.iter().all(|c| c.ok), "{checks:?}");
    }

    #[test]
    fn failures_name_the_offending_values() {
        let mut bad = good_proposal();
        bad.name = "no unwrap in libs!!".into();
        bad.description = "x".repeat(600);
        bad.citations = vec!["call_abc123".into()];
        bad.scripts[0].path = "../escape.py".into();
        bad.scripts[0].code = "import requests\nsrc = eval(\"x\")\nexec(src)".into();

        let checks = validate(&bad, &roster(), &[]);
        let failed: Vec<&str> = checks
            .iter()
            .filter(|c| !c.ok)
            .map(|c| c.msg.as_str())
            .collect();
        assert!(failed.iter().any(|m| m.contains("no unwrap in libs!!")));
        assert!(failed.iter().any(|m| m.contains("got 600")));
        assert!(failed.iter().any(|m| m.contains("call_abc123")));
        assert!(failed.iter().any(|m| m.contains("../escape.py")));
        assert!(failed
            .iter()
            .any(|m| m.contains("requests") && m.contains("eval") && m.contains("exec")));
    }

    #[test]
    fn empty_citations_and_empty_name_fail() {
        let mut bad = good_proposal();
        bad.name = String::new();
        bad.citations = Vec::new();
        let checks = validate(&bad, &roster(), &[]);
        assert!(checks.iter().filter(|c| !c.ok).count() >= 2);
    }

    #[test]
    fn a_name_already_installed_fails_with_the_collision_named() {
        let existing = vec!["retry-with-backoff".to_string()];
        let checks = validate(&good_proposal(), &roster(), &existing);
        assert!(checks
            .iter()
            .any(|c| !c.ok && c.msg.contains("already names an installed skill")));
    }

    #[test]
    fn shell_scripts_validate_against_their_own_allowlist() {
        let mut proposal = good_proposal();
        proposal.scripts.push(ScriptFile {
            path: "scripts/check_cap.sh".into(),
            code: "#!/bin/sh\n# comment lines are structure, not commands\n\
                   if grep -q 'sleep 0' \"$1\"; then\n  echo bad | sort && wc -l\nfi"
                .into(),
        });
        let checks = validate(&proposal, &roster(), &[]);
        assert!(checks.iter().all(|c| c.ok), "{checks:?}");
        assert!(checks
            .iter()
            .any(|c| c.msg.contains("check_cap.sh passes the shell allowlist")));
    }

    #[test]
    fn shell_violations_name_each_rejected_command() {
        let code = "curl http://x | grep y\nrm -rf /tmp/z; echo done\nFOO=1 sudo ls";
        let violations = shell_violations(code);
        assert_eq!(violations, vec!["curl", "rm", "sudo"]);
    }

    #[test]
    fn shell_structure_is_not_mistaken_for_commands() {
        // The exact shapes that false-positived in a real run: case-arm
        // patterns, a local function defined then invoked, and make.
        let code = "check() {\n  cargo clippy | head -5\n}\n\
                    case \"$1\" in\n  run|clippy|bench) cargo \"$1\" ;;\n  *) echo other ;;\nesac\n\
                    check\nmake lint\nfunction summarize() { git diff --stat; }\nsummarize";
        let violations = shell_violations(code);
        assert!(violations.is_empty(), "{violations:?}");

        // A banned command inside a case-arm body is still caught.
        let code = "case \"$1\" in\n  deploy) curl http://x ;;\nesac";
        assert_eq!(shell_violations(code), vec!["curl"]);
    }

    #[test]
    fn non_py_non_sh_script_paths_fail() {
        let mut proposal = good_proposal();
        proposal.scripts[0].path = "scripts/check.rb".into();
        let checks = validate(&proposal, &roster(), &[]);
        assert!(checks
            .iter()
            .any(|c| !c.ok && c.msg.contains("scripts/check.rb")));
    }

    #[test]
    fn allowlisted_idioms_are_not_rejected() {
        // The exact idioms that died one-by-one in v1 — lambda and open
        // included, which is why they are NOT on the banned list.
        let code = "import sys, collections, argparse\nfrom collections import defaultdict\n\
                    d = defaultdict(list)\nsrc = open(sys.argv[1]).read()\n\
                    rows = sorted(src.splitlines(), key=lambda l: len(l))\nsys.exit(0)";
        assert!(
            script_violations(code).is_empty(),
            "{:?}",
            script_violations(code)
        );
    }

    #[test]
    fn banned_names_in_comments_do_not_fail_the_scan() {
        let code = "#!/usr/bin/env python3\nimport sys  # never open or __import__ here\n\
                    # avoid eval, exec and lambda\nprint(sys.argv)";
        assert!(
            script_violations(code).is_empty(),
            "{:?}",
            script_violations(code)
        );
    }

    #[test]
    fn repair_feedback_lists_only_failures() {
        let checks = vec![
            Check::new(true, "fine"),
            Check::new(false, "name is kebab-case (got \"Bad Name\")"),
        ];
        let feedback = repair_feedback(&checks);
        assert!(feedback.contains("Bad Name"));
        assert!(!feedback.contains("fine"));
        assert!(feedback.contains("Resubmit"));
    }
}
