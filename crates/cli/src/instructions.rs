//! Project instructions: standing guidance the user writes once, in a
//! file, instead of retyping it into every prompt.
//!
//! Three files are read, in this order, and all of them apply:
//!
//! | File                          | Scope                          |
//! | :---------------------------- | :----------------------------- |
//! | `$ORCA_CONFIG_DIR/AGENTS.md`  | every workspace on this machine |
//! | `<workspace>/AGENTS.md`       | this project                    |
//! | `<workspace>/.orca/AGENTS.md` | this project, unshared          |
//!
//! `AGENTS.md` is the cross-agent convention, so a repository that
//! already has one works with `orcacode` without a second file. The
//! `.orca/` path is for guidance that belongs to one checkout rather
//! than to the repository.
//!
//! The result is appended to the system prompt by the caller — never
//! folded into `system_prompt()` itself, which is a fixed string with
//! tests asserting its contents. A file is read once, at startup: the
//! composed prompt is what `/clear` re-pushes, so an edited `AGENTS.md`
//! reaches the model on the next `orcacode`, not mid-session.

use std::path::{Path, PathBuf};

/// Per-file cap. Instructions share the context window with the actual
/// work, so a runaway file is truncated rather than allowed to crowd out
/// the conversation.
const MAX_BYTES: usize = 32 * 1024;

/// The file name looked for in every root.
pub const FILE_NAME: &str = "AGENTS.md";

/// One instruction file that was found and read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// How the file is named to the model and in the transcript.
    pub label: String,
    pub body: String,
    /// Bytes on disk before any cap was applied.
    pub bytes: usize,
    pub truncated: bool,
}

/// What a scan produced: the block to append to the system prompt, and
/// the lines to report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Instructions {
    pub sources: Vec<Source>,
}

impl Instructions {
    /// Read every instruction file that exists, in precedence order.
    /// Unreadable files are skipped silently: a directory named
    /// `AGENTS.md`, a broken symlink, or a permission error is not worth
    /// failing a session over.
    pub fn load(workspace: &Path) -> Self {
        let config_dir =
            crate::config::config_path().and_then(|path| path.parent().map(Path::to_path_buf));
        Self::from_roots(workspace, config_dir.as_deref())
    }

    /// Roots are passed in, not derived here, so tests can point a scan
    /// at temp directories without mutating process-global env vars.
    pub fn from_roots(workspace: &Path, config_dir: Option<&Path>) -> Self {
        let candidates = [
            config_dir.map(|dir| dir.join(FILE_NAME)),
            Some(workspace.join(FILE_NAME)),
            Some(workspace.join(".orca").join(FILE_NAME)),
        ];
        let mut sources = Vec::new();
        let mut seen: Vec<PathBuf> = Vec::new();
        for path in candidates.into_iter().flatten() {
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            if seen.contains(&canonical) {
                continue;
            }
            seen.push(canonical);
            if let Some(source) = read_source(&path, workspace) {
                sources.push(source);
            }
        }
        Self { sources }
    }

    /// The text to append to the system prompt, or `None` when no file
    /// was found. Empty files are dropped by the reader, so a block is
    /// never all preamble and no instructions.
    pub fn block(&self) -> Option<String> {
        if self.sources.is_empty() {
            return None;
        }
        let mut out = String::from(
            "\n\nThe user has left standing instructions for this workspace in the files below. \
             Follow them. Where they conflict with the general guidance above, they win; where \
             they conflict with what the user asks you right now, the user's request wins.",
        );
        for source in &self.sources {
            out.push_str("\n\n--- ");
            out.push_str(&source.label);
            out.push_str(" ---\n");
            out.push_str(source.body.trim_end());
        }
        Some(out)
    }

    /// One transcript line per file loaded, plus a warning for any file
    /// that had to be cut short. Silent when nothing was found.
    pub fn notices(&self) -> Vec<String> {
        self.sources
            .iter()
            .map(|source| {
                if source.truncated {
                    format!(
                        "instructions · {} ({}) · truncated at {}",
                        source.label,
                        size_label(source.bytes),
                        size_label(MAX_BYTES)
                    )
                } else {
                    format!(
                        "instructions · {} ({})",
                        source.label,
                        size_label(source.bytes)
                    )
                }
            })
            .collect()
    }
}

/// Read one candidate, or `None` when it is absent, unreadable, or has
/// nothing but whitespace in it.
fn read_source(path: &Path, workspace: &Path) -> Option<Source> {
    let text = std::fs::read_to_string(path).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    let bytes = text.len();
    let truncated = bytes > MAX_BYTES;
    let body = if truncated {
        // Cut on a char boundary: the cap is a size guard, not a parser.
        let mut end = MAX_BYTES;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text[..end].to_string()
    } else {
        text
    };
    Some(Source {
        label: label_for(path, workspace),
        body,
        bytes,
        truncated,
    })
}

/// Workspace files are named by their relative path (`AGENTS.md`,
/// `.orca/AGENTS.md`); anything outside keeps its full path, so the
/// machine-wide file is never mistaken for one in the repository.
fn label_for(path: &Path, workspace: &Path) -> String {
    path.strip_prefix(workspace)
        .map(|rel| rel.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn size_label(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} kB", bytes as f64 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "orca-cli-instructions-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, body: &str) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
            path
        }

        fn join(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn nothing_on_disk_is_silent_and_adds_no_block() {
        let temp = Temp::new("empty");
        fs::create_dir_all(temp.join("repo")).unwrap();
        let found = Instructions::from_roots(&temp.join("repo"), None);
        assert!(found.sources.is_empty());
        assert_eq!(found.block(), None);
        assert!(found.notices().is_empty());
    }

    #[test]
    fn all_three_roots_load_in_precedence_order() {
        let temp = Temp::new("roots");
        temp.write("config/AGENTS.md", "global rule\n");
        temp.write("repo/AGENTS.md", "project rule\n");
        temp.write("repo/.orca/AGENTS.md", "checkout rule\n");

        let found = Instructions::from_roots(&temp.join("repo"), Some(&temp.join("config")));
        let labels: Vec<&str> = found.sources.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                temp.join("config/AGENTS.md").display().to_string().as_str(),
                "AGENTS.md",
                ".orca/AGENTS.md",
            ]
        );

        let block = found.block().expect("a block");
        // The bodies appear in the same order, under their labels.
        let global = block.find("global rule").unwrap();
        let project = block.find("project rule").unwrap();
        let checkout = block.find("checkout rule").unwrap();
        assert!(global < project && project < checkout);
        assert!(block.contains("--- AGENTS.md ---"));
        assert!(block.contains("--- .orca/AGENTS.md ---"));
    }

    #[test]
    fn a_blank_file_is_not_an_instruction() {
        let temp = Temp::new("blank");
        temp.write("repo/AGENTS.md", "   \n\n\t\n");
        let found = Instructions::from_roots(&temp.join("repo"), None);
        assert!(found.sources.is_empty(), "whitespace is not guidance");
        assert_eq!(found.block(), None);
    }

    #[test]
    fn oversized_files_are_capped_and_reported() {
        let temp = Temp::new("cap");
        let body = "x".repeat(MAX_BYTES + 5_000);
        temp.write("repo/AGENTS.md", &body);

        let found = Instructions::from_roots(&temp.join("repo"), None);
        let source = &found.sources[0];
        assert!(source.truncated);
        assert_eq!(source.bytes, MAX_BYTES + 5_000);
        assert_eq!(source.body.len(), MAX_BYTES);
        assert!(found.notices()[0].contains("truncated"));
    }

    /// The cap must not slice a multi-byte character in half.
    #[test]
    fn capping_respects_char_boundaries() {
        let temp = Temp::new("utf8");
        // Three-byte chars: the cap lands mid-character for at least one
        // of these lengths whatever MAX_BYTES is.
        let body = "→".repeat(MAX_BYTES);
        temp.write("repo/AGENTS.md", &body);
        let found = Instructions::from_roots(&temp.join("repo"), None);
        let source = &found.sources[0];
        assert!(source.truncated);
        assert!(source.body.len() <= MAX_BYTES);
        assert!(source.body.chars().all(|c| c == '→'));
    }

    #[test]
    fn notices_name_each_file_and_its_size() {
        let temp = Temp::new("notices");
        temp.write("repo/AGENTS.md", "always run cargo fmt\n");
        let found = Instructions::from_roots(&temp.join("repo"), None);
        let notices = found.notices();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].starts_with("instructions · AGENTS.md ("));
        assert!(!notices[0].contains("truncated"));
    }

    /// A config directory that *is* the workspace must not load the same
    /// file twice.
    #[test]
    fn the_same_file_is_never_loaded_twice() {
        let temp = Temp::new("dedupe");
        temp.write("repo/AGENTS.md", "one rule\n");
        let repo = temp.join("repo");
        let found = Instructions::from_roots(&repo, Some(&repo));
        assert_eq!(found.sources.len(), 1);
    }

    /// The block tells the model how to rank the instructions against
    /// the base prompt and against a live request.
    #[test]
    fn the_block_states_precedence() {
        let temp = Temp::new("precedence");
        temp.write("repo/AGENTS.md", "prefer tabs\n");
        let block = Instructions::from_roots(&temp.join("repo"), None)
            .block()
            .unwrap();
        assert!(block.contains("Follow them"));
        assert!(block.contains("they win"));
        assert!(block.contains("the user's request wins"));
        assert!(block.contains("prefer tabs"));
    }
}
