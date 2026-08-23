//! Where a plan-mode session is allowed to put a plan.
//!
//! Plan mode without an artifact is just a refusal: the plan lands in the
//! assistant's prose and the next turn has to re-derive it. So plan mode
//! opens one writable area — `docs/plan/` in the workspace — and leaves
//! everything else read-only.
//!
//! **The agent decides whether to write a plan and what to call it.** The
//! host has no idea, at the point a plan-mode turn begins, whether the
//! conversation is a feature to design or a greeting; naming a file from
//! the first thing typed produces `docs/plan/2026-08-22-hi.md`. So the
//! host constrains the directory and the extension, tells the model the
//! naming convention and today's date, and stays out of the rest.
//!
//! Writing still goes through the ordinary `write_file` approval prompt,
//! so the user is asked before anything lands on disk. That composes
//! safely with an always-allow grant: in plan mode the gate refuses every
//! write outside this directory *before* approval sees it.

use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Workspace-relative directory holding plan artifacts. The only place a
/// plan-mode session may write.
pub const PLAN_DIR: &str = "docs/plan";

/// Whether `candidate` is a plan artifact: a markdown file directly
/// inside [`PLAN_DIR`].
///
/// Deliberately structural rather than a match against one host-chosen
/// name — the agent picks the filename. Nested paths are rejected so the
/// area stays one flat, listable directory, and `..` cannot appear at all
/// because normalization drops nothing and the prefix check would fail.
/// (`Workspace::resolve` already refuses escapes; this is the second
/// fence, not the only one.)
pub fn is_plan_path(candidate: &str) -> bool {
    let parts: Vec<&str> = candidate
        .split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    // docs / plan / <name>.md — exactly three segments, none of them `..`.
    let [dir, sub, name] = parts[..] else {
        return false;
    };
    let expected: Vec<&str> = PLAN_DIR.split('/').collect();
    dir == expected[0]
        && sub == expected[1]
        && name.len() > 3
        && name.ends_with(".md")
        && !name.starts_with('.')
}

/// One planning episode: whether the model has been told the rules, and
/// which plans it actually wrote. Cloneable — the gate records writes,
/// the TUI reports them when the episode ends.
#[derive(Clone, Default)]
pub struct PlanArea(Arc<RwLock<Episode>>);

#[derive(Default)]
struct Episode {
    briefed: bool,
    written: Vec<String>,
}

impl PlanArea {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim the right to brief the model, once per episode. Returns
    /// `true` the first time, `false` afterwards.
    pub fn open(&self) -> bool {
        let mut episode = self.0.write().expect("plan area lock");
        if episode.briefed {
            return false;
        }
        episode.briefed = true;
        true
    }

    /// Note a plan the agent actually wrote. Recorded from the tool's
    /// result, not from the call, so a write the user denied or that
    /// failed is never reported as a saved plan. Idempotent: rewriting
    /// the same plan does not list it twice.
    pub fn record(&self, path: &str) {
        let mut episode = self.0.write().expect("plan area lock");
        if !episode.written.iter().any(|seen| seen == path) {
            episode.written.push(path.to_string());
        }
    }

    /// The plans written so far this episode.
    pub fn written(&self) -> Vec<String> {
        self.0.read().expect("plan area lock").written.clone()
    }

    /// End the episode, returning what was written. The next planning
    /// session briefs again and starts a fresh list.
    pub fn end(&self) -> Vec<String> {
        let mut episode = self.0.write().expect("plan area lock");
        episode.briefed = false;
        std::mem::take(&mut episode.written)
    }
}

/// Today as `YYYY-MM-DD`, UTC.
pub fn today() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    date_from_unix(seconds)
}

/// `YYYY-MM-DD` (UTC) from unix seconds — Howard Hinnant's
/// civil-from-days. Inlined rather than pulling in a date crate: the
/// binary size is a selling point, and this is the only date arithmetic
/// in the workspace.
pub fn date_from_unix(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

/// The system message that opens a planning episode: the restriction, the
/// one writable area, the naming convention, and — explicitly — that
/// deciding whether a plan is warranted at all is the model's call.
pub fn briefing(today: &str) -> String {
    format!(
        "Plan mode is on: this session may read, search, and fetch, but may change nothing — \
         no shell, no process, no pykernel, no subagents, and no writes anywhere except the \
         `{PLAN_DIR}/` directory.\n\
         \n\
         When you have designed something worth building, write the plan to \
         `{PLAN_DIR}/{today}-<feature-name>.md` — dated today, named after the feature you are \
         planning, in kebab-case. Use write_file to create it and edit_file to revise it. You \
         choose the name; make it describe the work, not the conversation.\n\
         \n\
         Decide for yourself whether a plan is warranted. A greeting, a question, or a request \
         to explain something needs no file — answer it. Write a plan when the user has asked \
         for work that spans several steps, and only once you have investigated enough with \
         read_file, list_dir, grep, and glob to know what it should say.\n\
         \n\
         Structure a plan as: a one-sentence goal, two or three sentences on the approach, the \
         files each step creates or modifies, and the steps themselves as `- [ ]` checkboxes \
         small enough to do one at a time, each saying what to verify. The user approves the \
         file before it is created, and leaves plan mode with /mode."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_match_the_civil_calendar() {
        assert_eq!(date_from_unix(0), "1970-01-01");
        assert_eq!(date_from_unix(86_399), "1970-01-01");
        assert_eq!(date_from_unix(86_400), "1970-01-02");
        // A leap day, and the day after.
        assert_eq!(date_from_unix(1_709_164_800), "2024-02-29");
        assert_eq!(date_from_unix(1_709_251_200), "2024-03-01");
        // A mid-2026 timestamp, cross-checked against the civil calendar.
        assert_eq!(date_from_unix(1_787_000_000), "2026-08-17");
        // Year boundaries.
        assert_eq!(date_from_unix(1_767_225_599), "2025-12-31");
        assert_eq!(date_from_unix(1_767_225_600), "2026-01-01");
    }

    /// Whatever the agent names it, it has to be a markdown file sitting
    /// directly in the plan directory.
    #[test]
    fn plan_paths_are_markdown_files_in_the_plan_directory() {
        for path in [
            "docs/plan/2026-08-22-add-rewind.md",
            "./docs/plan/2026-08-22-add-rewind.md",
            "docs//plan/anything-at-all.md",
            "docs/plan/UPPERCASE.md",
        ] {
            assert!(is_plan_path(path), "{path} should be a plan");
        }
    }

    /// The area is a fence, so everything outside it — including clever
    /// spellings of outside — is not a plan.
    #[test]
    fn nothing_outside_the_plan_directory_is_a_plan() {
        for path in [
            // Wrong place.
            "src/main.rs",
            "docs/design.html",
            "docs/superpowers/plans/x.md",
            "plan/x.md",
            "docs/plans/x.md",
            // Escapes, however written.
            "docs/plan/../../src/main.rs",
            "docs/plan/../secrets.md",
            "../docs/plan/x.md",
            // Nested: the area stays one flat directory.
            "docs/plan/sub/x.md",
            // Not a markdown file.
            "docs/plan/x.rs",
            "docs/plan/Cargo.toml",
            "docs/plan/x",
            // The directory itself, and degenerate names.
            "docs/plan",
            "docs/plan/",
            "docs/plan/.md",
            "docs/plan/.hidden.md",
            "",
        ] {
            assert!(!is_plan_path(path), "{path} must not be a plan");
        }
    }

    #[test]
    fn an_episode_briefs_once_and_collects_what_was_written() {
        let area = PlanArea::new();
        assert!(area.open(), "the first turn briefs");
        assert!(!area.open(), "later turns do not");
        assert!(area.written().is_empty());

        area.record("docs/plan/2026-08-22-a.md");
        area.record("docs/plan/2026-08-22-b.md");
        // Revising a plan does not list it twice.
        area.record("docs/plan/2026-08-22-a.md");
        assert_eq!(
            area.written(),
            [
                "docs/plan/2026-08-22-a.md".to_string(),
                "docs/plan/2026-08-22-b.md".to_string()
            ]
        );

        // Ending hands back what was written and resets for next time.
        let written = area.end();
        assert_eq!(written.len(), 2);
        assert!(area.written().is_empty());
        assert!(area.open(), "a second episode briefs again");
    }

    #[test]
    fn clones_share_one_episode() {
        let area = PlanArea::new();
        let gate_view = area.clone();
        gate_view.record("docs/plan/x.md");
        assert_eq!(area.written(), ["docs/plan/x.md".to_string()]);
        area.end();
        assert!(gate_view.written().is_empty());
    }

    /// The briefing has to leave the decision with the model, and say
    /// enough for it to name the file the way the convention wants.
    #[test]
    fn the_briefing_hands_the_decision_to_the_model() {
        let text = briefing("2026-08-22");
        assert!(
            text.contains("docs/plan/2026-08-22-<feature-name>.md"),
            "{text}"
        );
        assert!(text.contains("You choose the name"), "{text}");
        assert!(text.contains("Decide for yourself"), "{text}");
        assert!(text.contains("needs no file"), "{text}");
        assert!(text.contains("write_file"));
        assert!(text.contains("edit_file"));
        assert!(text.contains("- [ ]"));
        // It must not promise a specific file exists or is expected.
        assert!(!text.contains("That is the only path you may write"));
    }
}
