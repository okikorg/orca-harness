//! Parser and exact hunk application for the model-facing patch format.

use std::collections::BTreeSet;

use orca_harness_core::ToolError;

use super::files::{capped_list, AMBIGUOUS_LINES_SHOWN};

const MAX_OPERATIONS: usize = 256;
const MAX_PATCH_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub(super) enum PatchAction {
    Add { path: String, lines: Vec<String> },
    Update { path: String, hunks: Vec<Hunk> },
    Delete { path: String },
}

impl PatchAction {
    pub(super) fn path(&self) -> &str {
        match self {
            Self::Add { path, .. } | Self::Update { path, .. } | Self::Delete { path } => path,
        }
    }
}

#[derive(Debug)]
pub(super) struct Hunk {
    lines: Vec<PatchLine>,
}

impl Hunk {
    /// A hunk with only context lines changes nothing: it seeks forward to
    /// narrow where the following hunk may match.
    fn is_anchor(&self) -> bool {
        self.lines.iter().all(|line| line.kind == ' ')
    }
}

#[derive(Debug)]
struct PatchLine {
    kind: char,
    text: String,
}

pub(super) fn parse_patch(patch: &str) -> Result<Vec<PatchAction>, ToolError> {
    if patch.len() > MAX_PATCH_BYTES {
        return Err(ToolError::msg(format!(
            "patch exceeds the {MAX_PATCH_BYTES}-byte limit"
        )));
    }
    let lines: Vec<&str> = patch
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if lines.first() != Some(&"*** Begin Patch") || lines.last() != Some(&"*** End Patch") {
        return Err(ToolError::msg(
            "patch must start with `*** Begin Patch` and end with `*** End Patch`",
        ));
    }
    let mut actions = Vec::new();
    let mut index = 1usize;
    while index + 1 < lines.len() {
        let header = lines[index];
        index += 1;
        if let Some(path) = header.strip_prefix("*** Add File: ") {
            let mut added = Vec::new();
            while index + 1 < lines.len() && !lines[index].starts_with("*** ") {
                let line = lines[index].strip_prefix('+').ok_or_else(|| {
                    ToolError::msg(format!(
                        "add-file content line {} must start with `+`",
                        index + 1
                    ))
                })?;
                added.push(line.to_owned());
                index += 1;
            }
            actions.push(PatchAction::Add {
                path: path.to_owned(),
                lines: added,
            });
        } else if let Some(path) = header.strip_prefix("*** Delete File: ") {
            actions.push(PatchAction::Delete {
                path: path.to_owned(),
            });
        } else if let Some(path) = header.strip_prefix("*** Update File: ") {
            let mut hunks = Vec::new();
            while index + 1 < lines.len() && !lines[index].starts_with("*** ") {
                if !lines[index].starts_with("@@") {
                    return Err(ToolError::msg(format!(
                        "update-file line {} must start a hunk with `@@`",
                        index + 1
                    )));
                }
                index += 1;
                let mut hunk_lines = Vec::new();
                while index + 1 < lines.len()
                    && !lines[index].starts_with("@@")
                    && !lines[index].starts_with("*** ")
                {
                    let raw = lines[index];
                    let kind = raw.chars().next().ok_or_else(|| {
                        ToolError::msg(format!("empty patch line at {}", index + 1))
                    })?;
                    if !matches!(kind, ' ' | '+' | '-') {
                        return Err(ToolError::msg(format!(
                            "patch line {} must start with space, `+`, or `-`",
                            index + 1
                        )));
                    }
                    hunk_lines.push(PatchLine {
                        kind,
                        text: raw[kind.len_utf8()..].to_owned(),
                    });
                    index += 1;
                }
                // A context-only hunk is allowed: models emit `@@` blocks with
                // nothing but context as position anchors for the hunk after.
                if hunk_lines.is_empty() {
                    return Err(ToolError::msg(format!("empty hunk at line {index}")));
                }
                hunks.push(Hunk { lines: hunk_lines });
            }
            if hunks.is_empty() {
                return Err(ToolError::msg(format!("update for {path} has no hunks")));
            }
            actions.push(PatchAction::Update {
                path: path.to_owned(),
                hunks,
            });
        } else {
            return Err(ToolError::msg(format!("unknown patch header `{header}`")));
        }
    }
    if actions.is_empty() {
        return Err(ToolError::msg("patch contains no file operations"));
    }
    if actions.len() > MAX_OPERATIONS {
        return Err(ToolError::msg(format!(
            "patch exceeds the {MAX_OPERATIONS}-file operation limit"
        )));
    }
    let mut unique = BTreeSet::new();
    for action in &actions {
        if action.path().is_empty() {
            return Err(ToolError::msg("patch paths must not be empty"));
        }
        if !unique.insert(action.path()) {
            return Err(ToolError::msg(format!(
                "patch contains duplicate operation for {}",
                action.path()
            )));
        }
    }
    Ok(actions)
}

#[derive(Clone, Copy)]
enum DocumentLine<'content, 'patch> {
    Original(&'content str),
    Patch(&'patch str),
}

impl DocumentLine<'_, '_> {
    fn text(&self) -> &str {
        match self {
            Self::Original(text) => text,
            Self::Patch(text) => text,
        }
    }
}

struct TextDocument<'content, 'patch> {
    lines: Vec<DocumentLine<'content, 'patch>>,
    trailing_newline: bool,
    crlf: bool,
}

impl<'content, 'patch> TextDocument<'content, 'patch> {
    fn parse(content: &'content str) -> Self {
        let crlf = content.contains("\r\n");
        let trailing_newline = content.ends_with('\n');
        let mut lines: Vec<DocumentLine<'content, 'patch>> = content
            .split('\n')
            .map(|line| DocumentLine::Original(line.strip_suffix('\r').unwrap_or(line)))
            .collect();
        if trailing_newline {
            lines.pop();
        }
        Self {
            lines,
            trailing_newline,
            crlf,
        }
    }

    fn render(self) -> String {
        let separator = if self.crlf { "\r\n" } else { "\n" };
        let capacity = self
            .lines
            .iter()
            .map(|line| line.text().len())
            .sum::<usize>()
            + separator.len() * self.lines.len();
        let mut content = String::with_capacity(capacity);
        for (index, line) in self.lines.iter().enumerate() {
            if index > 0 {
                content.push_str(separator);
            }
            content.push_str(line.text());
        }
        if self.trailing_newline {
            content.push_str(separator);
        }
        content
    }
}

pub(super) fn apply_hunks(path: &str, content: &str, hunks: &[Hunk]) -> Result<String, ToolError> {
    let mut document = TextDocument::parse(content);
    let mut cursor = 0usize;
    for (hunk_index, hunk) in hunks.iter().enumerate() {
        let old_len = hunk.lines.iter().filter(|line| line.kind != '+').count();
        if old_len == 0 {
            return Err(ToolError::msg(format!(
                "hunk {} for {path} has no context or removed lines",
                hunk_index + 1
            )));
        }
        if old_len > document.lines.len().saturating_sub(cursor) {
            return Err(ToolError::msg(format!(
                "hunk {} for {path} did not match the current file",
                hunk_index + 1
            )));
        }
        let mut matches = (cursor..=document.lines.len() - old_len).filter(|&start| {
            document.lines[start..start + old_len]
                .iter()
                .map(|line| line.text())
                .eq(hunk
                    .lines
                    .iter()
                    .filter(|line| line.kind != '+')
                    .map(|line| line.text.as_str()))
        });
        let start = matches.next().ok_or_else(|| {
            ToolError::msg(format!(
                "hunk {} for {path} did not match the current file",
                hunk_index + 1
            ))
        })?;
        // Hunks are ordered, and the order is the disambiguation: once a
        // hunk or anchor has placed the cursor, the next hunk is the nearest
        // match after it, exactly as the model that wrote the patch expects.
        // Only a file's first hunk has nothing before it to order it, so it
        // alone must be unique, unless it is an anchor: anchors are nearest-
        // match by definition (a lone `}` recurs throughout a file).
        let anchor = hunk.is_anchor();
        if hunk_index == 0 && !anchor {
            let lines: Vec<usize> = std::iter::once(start)
                .chain(matches)
                .map(|line| line + 1)
                .collect();
            if lines.len() > 1 {
                return Err(ToolError::msg(format!(
                    "hunk 1 for {path} matched {} locations (lines {}); include more context or an anchor hunk before it",
                    lines.len(),
                    capped_list(&lines, AMBIGUOUS_LINES_SHOWN)
                )));
            }
        }
        if anchor {
            cursor = start + old_len;
            continue;
        }
        let new = hunk
            .lines
            .iter()
            .filter(|line| line.kind != '-')
            .map(|line| DocumentLine::Patch(line.text.as_str()));
        let new_len = hunk.lines.iter().filter(|line| line.kind != '-').count();
        document.lines.splice(start..start + old_len, new);
        cursor = start + new_len;
    }
    Ok(document.render())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_parser_rejects_duplicate_paths() {
        let error = parse_patch(
            "*** Begin Patch\n*** Delete File: a.txt\n*** Delete File: a.txt\n*** End Patch",
        )
        .unwrap_err();
        assert!(error.message.contains("duplicate operation"));
    }

    #[test]
    fn text_document_preserves_crlf_and_final_newline() {
        let document = TextDocument::parse("a\r\nb\r\n");
        assert_eq!(document.render(), "a\r\nb\r\n");
    }

    fn update_hunks(patch: &str) -> Vec<Hunk> {
        match parse_patch(patch).unwrap().pop().unwrap() {
            PatchAction::Update { hunks, .. } => hunks,
            other => panic!("expected an update, got {other:?}"),
        }
    }

    #[test]
    fn context_only_hunk_anchors_the_following_hunk() {
        // `-    x` alone matches both functions; the anchor scopes it to `b`.
        let content = "fn a() {\n    x\n}\nfn b() {\n    x\n}\n";
        let hunks = update_hunks(
            "*** Begin Patch\n*** Update File: f.rs\n@@\n fn b() {\n@@\n-    x\n+    y\n*** End Patch",
        );
        let after = apply_hunks("f.rs", content, &hunks).unwrap();
        assert_eq!(after, "fn a() {\n    x\n}\nfn b() {\n    y\n}\n");
    }

    #[test]
    fn anchor_seeks_the_first_match_after_the_cursor() {
        // `    }` closes both methods; an anchor takes the nearest one rather
        // than demanding uniqueness, and only the real hunk must be unique.
        let content =
            "impl A {\n    fn new() {\n        a\n    }\n    fn other() {\n        b\n    }\n}\n";
        let hunks = update_hunks(
            "*** Begin Patch\n*** Update File: f.rs\n@@\n         a\n@@\n     }\n@@\n-    fn other() {\n+    fn renamed() {\n*** End Patch",
        );
        let after = apply_hunks("f.rs", content, &hunks).unwrap();
        assert_eq!(
            after,
            "impl A {\n    fn new() {\n        a\n    }\n    fn renamed() {\n        b\n    }\n}\n"
        );
    }

    #[test]
    fn anchor_then_append_after_closing_brace() {
        // The shape the model emits to add an item after an existing one.
        let content = "fn paste() {\n    body\n}\n\n/// docs\nfn next() {}\n";
        let hunks = update_hunks(
            "*** Begin Patch\n*** Update File: f.rs\n@@\n fn paste() {\n@@\n }\n+\n+#[test]\n+fn added() {}\n \n /// docs\n*** End Patch",
        );
        let after = apply_hunks("f.rs", content, &hunks).unwrap();
        assert_eq!(
            after,
            "fn paste() {\n    body\n}\n\n#[test]\nfn added() {}\n\n/// docs\nfn next() {}\n"
        );
    }

    #[test]
    fn later_hunks_take_the_nearest_match_after_the_previous_one() {
        // Three match arms share an identical block. A hunk that pins arm B
        // followed by the shared block must edit B's copy, not fail because
        // C's copy also lies ahead of the cursor.
        let content = "A => {\n    x\n}\nB => {\n    x\n}\nC => {\n    x\n}\n";
        let hunks = update_hunks(
            "*** Begin Patch\n*** Update File: f.rs\n@@\n-B => {\n+B2 => {\n@@\n-    x\n+    y\n*** End Patch",
        );
        let after = apply_hunks("f.rs", content, &hunks).unwrap();
        assert_eq!(
            after,
            "A => {\n    x\n}\nB2 => {\n    y\n}\nC => {\n    x\n}\n"
        );
    }

    #[test]
    fn a_files_first_hunk_must_be_unique_and_the_error_names_the_lines() {
        let hunks = update_hunks(
            "*** Begin Patch\n*** Update File: f.rs\n@@\n-same\n+changed\n*** End Patch",
        );
        let error = apply_hunks("f.rs", "same\nother\nsame\n", &hunks).unwrap_err();
        assert!(
            error.message.contains("matched 2 locations (lines 1, 3)"),
            "{}",
            error.message
        );
    }

    #[test]
    fn anchor_that_matches_nothing_is_an_error() {
        let hunks = update_hunks(
            "*** Begin Patch\n*** Update File: f.rs\n@@\n fn missing() {\n@@\n-a\n+b\n*** End Patch",
        );
        let error = apply_hunks("f.rs", "a\n", &hunks).unwrap_err();
        assert!(error.message.contains("hunk 1 for f.rs did not match"));
    }

    #[test]
    fn empty_hunk_is_rejected() {
        let error =
            parse_patch("*** Begin Patch\n*** Update File: f.rs\n@@\n@@\n-a\n+b\n*** End Patch")
                .unwrap_err();
        assert_eq!(error.message, "empty hunk at line 3");
    }
}
