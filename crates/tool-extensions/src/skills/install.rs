//! Getting skills onto disk: scaffold a new one, copy one in from a
//! folder or a git repository, delete one again.
//!
//! Source syntax follows the `npx skills` CLI, which is what the
//! ecosystem has settled on (`owner/repo`, `owner/repo@skill`, a GitHub
//! or GitLab URL, a `/tree/<ref>/<path>` deep link, a local path), plus
//! the `--skill <name>` filter. A pasted `npx skills add …` command is
//! accepted verbatim, because that is what a README hands you.
//!
//! Two rules the rest of the design leans on:
//!
//! - **Copy, never symlink.** The `skill` tool contains `resource`
//!   reads to the skill directory by canonicalizing; a tree of symlinks
//!   pointing back at the source would make that check meaningful only
//!   by accident. Symlinks inside a source are skipped for the same
//!   reason.
//! - **Install is user-driven.** There is no install tool for the model.
//!   A skill body becomes model instructions, so what gets installed is
//!   a decision for the person at the keyboard.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::process::Command;

/// Files copied for one skill. A skill is documentation; a source that
/// wants to ship a tree this large is shipping something else.
const MAX_FILES: usize = 200;

/// Bytes copied for one skill.
const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// How long `git clone` may take before it is killed.
const CLONE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// Directory levels searched for `SKILL.md` inside a source, matching
/// the depth the `npx skills` CLI searches.
const MAX_SCAN_DEPTH: usize = 3;

/// Where a skill is being copied from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Local(PathBuf),
    /// A repository to shallow-clone, optionally narrowed to a
    /// subdirectory by a `/tree/<ref>/<path>` deep link.
    Git {
        url: String,
        subdir: Option<String>,
    },
}

/// A parsed `/skills add` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub origin: Origin,
    /// Install only this skill out of a multi-skill source.
    pub filter: Option<String>,
    /// `--list`: report what the source holds, install nothing.
    pub list_only: bool,
}

/// One skill found in a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub dir: PathBuf,
}

/// Read `/skills add` arguments the way the `npx skills` CLI reads them.
pub fn parse_request(raw: &str) -> Result<Request, String> {
    // A README says `npx skills add owner/repo --skill x`; pasting that
    // whole line after `/skills add` should work.
    let mut rest = raw.trim();
    for prefix in ["npx skills add", "npx -y skills add", "skills add"] {
        if let Some(stripped) = rest.strip_prefix(prefix) {
            rest = stripped.trim_start();
            break;
        }
    }

    let mut source: Option<String> = None;
    let mut filter: Option<String> = None;
    let mut list_only = false;
    let mut tokens = rest.split_whitespace();
    while let Some(token) = tokens.next() {
        match token {
            "--skill" | "-s" => {
                filter = Some(
                    tokens
                        .next()
                        .ok_or("`--skill` needs a name")?
                        .trim_matches('\'')
                        .trim_matches('"')
                        .to_string(),
                )
            }
            "--list" | "-l" => list_only = true,
            // Flags the ecosystem CLI takes that mean nothing here: this
            // is one agent, and the destination is chosen by /skills add
            // itself. Ignored rather than rejected, so a pasted command
            // still works.
            "-g" | "--global" | "-y" | "--yes" | "--copy" | "--all" => {}
            "-a" | "--agent" => {
                tokens.next();
            }
            other if other.starts_with("--skill=") => {
                filter = Some(other["--skill=".len()..].to_string())
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag: {other}"));
            }
            other if source.is_none() => source = Some(other.to_string()),
            other => return Err(format!("unexpected argument: {other}")),
        }
    }

    let source = source.ok_or("a source is required")?;
    let (origin, embedded) = parse_origin(&source)?;
    Ok(Request {
        origin,
        filter: filter.or(embedded),
        list_only,
    })
}

/// The source token: a local path, a URL, `owner/repo`, or
/// `owner/repo@skill` (which also names the skill).
fn parse_origin(source: &str) -> Result<(Origin, Option<String>), String> {
    if source.starts_with('.') || source.starts_with('/') || source.starts_with('~') {
        let expanded = match source.strip_prefix("~/") {
            Some(rest) => match std::env::var_os("HOME") {
                Some(home) => PathBuf::from(home).join(rest),
                None => return Err("no home directory to expand ~".into()),
            },
            None => PathBuf::from(source),
        };
        return Ok((Origin::Local(expanded), None));
    }
    if source.starts_with("http://") || source.starts_with("https://") {
        return Ok(parse_url(source));
    }
    if source.starts_with("git@") {
        return Ok((
            Origin::Git {
                url: source.to_string(),
                subdir: None,
            },
            None,
        ));
    }
    // `owner/repo` or `owner/repo@skill`.
    let (repo, skill) = match source.split_once('@') {
        Some((repo, skill)) => (repo, Some(skill.to_string())),
        None => (source, None),
    };
    let slashes = repo.matches('/').count();
    if slashes != 1 || repo.starts_with('/') || repo.ends_with('/') {
        return Err(format!(
            "unrecognized source: {source} — expected owner/repo, a URL, or a local path"
        ));
    }
    Ok((
        Origin::Git {
            url: format!("https://github.com/{repo}.git"),
            subdir: None,
        },
        skill,
    ))
}

/// A web URL: `skills.sh/owner/repo` names a GitHub repository, and a
/// `/tree/<ref>/<path>` deep link narrows the clone to one directory.
fn parse_url(source: &str) -> (Origin, Option<String>) {
    if let Some(rest) = source.split_once("skills.sh/").map(|(_, rest)| rest) {
        let mut parts = rest.trim_end_matches('/').split('/');
        if let (Some(owner), Some(repo)) = (parts.next(), parts.next()) {
            return (
                Origin::Git {
                    url: format!("https://github.com/{owner}/{repo}.git"),
                    subdir: None,
                },
                parts.next().map(str::to_string),
            );
        }
    }
    if let Some((base, rest)) = source.split_once("/tree/") {
        // rest is `<ref>/<path…>`; the ref is not honored (the clone is
        // shallow from the default branch), the path is.
        let subdir = rest.split_once('/').map(|(_, path)| path.to_string());
        return (
            Origin::Git {
                url: format!("{base}.git"),
                subdir,
            },
            None,
        );
    }
    (
        Origin::Git {
            url: format!(
                "{}.git",
                source.trim_end_matches('/').trim_end_matches(".git")
            ),
            subdir: None,
        },
        None,
    )
}

/// Every directory holding a `SKILL.md`, breadth-limited the way the
/// ecosystem CLI limits it. Sorted by name.
pub fn find_candidates(root: &Path) -> Vec<Candidate> {
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if dir.join("SKILL.md").is_file() {
            if let Some(name) = dir.file_name() {
                found.push(Candidate {
                    name: name.to_string_lossy().into_owned(),
                    dir: dir.clone(),
                });
            }
            continue;
        }
        if depth >= MAX_SCAN_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            // `.claude/skills` and friends are exactly where a repo
            // keeps its skills, so hidden directories are searched — but
            // never `.git`, which is large and holds no skills.
            if path.is_dir() && path.file_name() != Some(std::ffi::OsStr::new(".git")) {
                stack.push((path, depth + 1));
            }
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found.dedup_by(|a, b| a.name == b.name);
    found
}

/// A source made readable. `root` is where the skills are; `cleanup` is
/// the temp tree to delete on drop, which for a `/tree/<ref>/<path>`
/// link is the clone above `root` rather than `root` itself.
pub struct Checkout {
    pub root: PathBuf,
    cleanup: Option<PathBuf>,
}

impl Drop for Checkout {
    fn drop(&mut self) {
        if let Some(dir) = &self.cleanup {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Put the source somewhere readable: a local path is used in place, a
/// repository is shallow-cloned into a temp directory.
pub async fn checkout(origin: &Origin) -> Result<Checkout, String> {
    match origin {
        Origin::Local(path) => {
            if !path.is_dir() {
                return Err(format!("not a directory: {}", path.display()));
            }
            Ok(Checkout {
                root: path.clone(),
                cleanup: None,
            })
        }
        Origin::Git { url, subdir } => {
            let into = scratch_dir("clone");
            let clone = Command::new("git")
                .args(["clone", "--depth", "1", "--quiet", url])
                .arg(&into)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .output();
            let output = match tokio::time::timeout(CLONE_TIMEOUT, clone).await {
                Err(_) => {
                    let _ = std::fs::remove_dir_all(&into);
                    return Err(format!("git clone timed out after {CLONE_TIMEOUT:?}"));
                }
                Ok(Err(err)) => {
                    let _ = std::fs::remove_dir_all(&into);
                    return Err(format!("could not run git: {err}"));
                }
                Ok(Ok(output)) => output,
            };
            if !output.status.success() {
                let _ = std::fs::remove_dir_all(&into);
                let why = String::from_utf8_lossy(&output.stderr);
                let why = why.lines().last().unwrap_or("clone failed").trim();
                return Err(format!("git clone failed: {why}"));
            }
            // A deep link narrows what is searched; the clone above it
            // stays the thing that gets cleaned up.
            let checkout = Checkout {
                root: match subdir {
                    Some(subdir) => safe_join(&into, subdir)?,
                    None => into.clone(),
                },
                cleanup: Some(into),
            };
            if !checkout.root.is_dir() {
                return Err(format!(
                    "no such path in the repository: {}",
                    subdir.as_deref().unwrap_or_default()
                ));
            }
            Ok(checkout)
        }
    }
}

/// One skill copied into place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub name: String,
    pub path: PathBuf,
}

/// Copy `candidate` into `dest_root/<name>`, atomically enough that a
/// failed copy never leaves a half-written skill behind.
pub fn install(candidate: &Candidate, dest_root: &Path) -> Result<Installed, String> {
    let target = dest_root.join(&candidate.name);
    if target.exists() {
        return Err(format!(
            "{} is already installed — /skills remove {} first",
            candidate.name, candidate.name
        ));
    }
    std::fs::create_dir_all(dest_root)
        .map_err(|e| format!("cannot create {}: {e}", dest_root.display()))?;
    let staging = dest_root.join(format!(".{}.incoming-{}", candidate.name, unique()));
    let result = copy_tree(&candidate.dir, &staging);
    if let Err(err) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&staging, &target) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!("cannot install {}: {err}", candidate.name));
    }
    Ok(Installed {
        name: candidate.name.clone(),
        path: target,
    })
}

/// Write a starter `SKILL.md`, refusing to overwrite an existing skill.
pub fn scaffold(dest_root: &Path, name: &str) -> Result<PathBuf, String> {
    let dir = dest_root.join(name);
    let file = dir.join("SKILL.md");
    if file.exists() {
        return Err(format!("{name} already exists at {}", file.display()));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let template = format!(
        "---\n\
         name: {name}\n\
         description: One line on when the agent should read this. This is all it \
         sees before deciding to load the skill.\n\
         ---\n\
         \n\
         # {name}\n\
         \n\
         Write the procedure here: the steps, the commands to run, the things that \
         are easy to get wrong. Point at other files in this folder and the agent \
         can load them too.\n"
    );
    std::fs::write(&file, template).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    Ok(file)
}

/// Delete an installed skill directory.
pub fn uninstall(dir: &Path) -> Result<(), String> {
    std::fs::remove_dir_all(dir).map_err(|e| format!("cannot remove {}: {e}", dir.display()))
}

/// Recursive copy with both caps enforced, skipping symlinks and `.git`.
fn copy_tree(src: &Path, dest: &Path) -> Result<(), String> {
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut stack = vec![(src.to_path_buf(), dest.to_path_buf())];
    while let Some((from, to)) = stack.pop() {
        std::fs::create_dir_all(&to).map_err(|e| format!("cannot create {}: {e}", to.display()))?;
        let entries =
            std::fs::read_dir(&from).map_err(|e| format!("cannot read {}: {e}", from.display()))?;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let name = entry.file_name();
            if name == std::ffi::OsStr::new(".git") {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            // `metadata()` on a DirEntry does not follow symlinks, so
            // this is the link itself: skip it rather than copy what it
            // points at, which may be anywhere on the machine.
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push((path, to.join(name)));
                continue;
            }
            files += 1;
            bytes += meta.len();
            if files > MAX_FILES || bytes > MAX_BYTES {
                return Err(format!(
                    "source is too large for a skill (over {MAX_FILES} files or \
                     {} MiB) — copy in just the folder you want",
                    MAX_BYTES / (1024 * 1024)
                ));
            }
            std::fs::copy(&path, to.join(name))
                .map_err(|e| format!("cannot copy {}: {e}", path.display()))?;
        }
    }
    Ok(())
}

fn scratch_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("orca-skill-{tag}-{}", unique()))
}

/// Process id plus a counter: unique per call without a rng dependency.
fn unique() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Join a caller-supplied relative path, refusing anything that climbs.
fn safe_join(base: &Path, rel: &str) -> Result<PathBuf, String> {
    let mut out = base.to_path_buf();
    for component in Path::new(rel).components() {
        match component {
            std::path::Component::Normal(part) => out.push(part),
            std::path::Component::CurDir => {}
            _ => return Err(format!("unusable path: {rel}")),
        }
    }
    Ok(out)
}

#[cfg(test)]
#[path = "install/tests.rs"]
mod tests;
