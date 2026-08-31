//! Composer location-mention parsing and workspace path discovery.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use super::format::byte_index;
use super::state::LocationEntry;

pub(crate) fn mention_starts_at(composer: &str, at: usize) -> bool {
    at == 0
        || composer
            .chars()
            .nth(at.saturating_sub(1))
            .is_some_and(char::is_whitespace)
}

/// Remove the complete `@path` token immediately before the cursor. The
/// picker inserts one trailing space, which is removed with the mention so a
/// single Backspace cleanly undoes the selection.
pub(crate) fn remove_location_mention_before_cursor(
    composer: &mut String,
    cursor: &mut usize,
) -> bool {
    remove_mention_before_cursor(composer, cursor, '@', |_| true)
}

/// Remove a complete `$skill-name ` token immediately before the cursor.
/// Names are validated during skill discovery, so a whitespace-delimited
/// token is the same boundary the picker inserted.
pub(crate) fn remove_skill_mention_before_cursor(
    composer: &mut String,
    cursor: &mut usize,
    is_skill: impl FnOnce(&str) -> bool,
) -> bool {
    remove_mention_before_cursor(composer, cursor, '$', is_skill)
}

fn remove_mention_before_cursor(
    composer: &mut String,
    cursor: &mut usize,
    marker: char,
    accept: impl FnOnce(&str) -> bool,
) -> bool {
    if *cursor == 0 {
        return false;
    }
    let chars: Vec<char> = composer.chars().collect();
    let token_end = if chars.get(*cursor - 1).is_some_and(|c| c.is_whitespace()) {
        *cursor - 1
    } else {
        *cursor
    };
    if token_end == 0 {
        return false;
    }
    let token_start = chars[..token_end]
        .iter()
        .rposition(|c| c.is_whitespace())
        .map_or(0, |index| index + 1);
    if chars.get(token_start) != Some(&marker) || token_end == token_start + 1 {
        return false;
    }

    let start_byte = byte_index(composer, token_start);
    let name_start_byte = byte_index(composer, token_start + 1);
    let token_end_byte = byte_index(composer, token_end);
    if !accept(&composer[name_start_byte..token_end_byte]) {
        return false;
    }
    let end_byte = byte_index(composer, *cursor);
    composer.replace_range(start_byte..end_byte, "");
    *cursor = token_start;
    true
}

/// Enumerate a bounded set of workspace-relative paths without following
/// symlinks. Build/dependency metadata directories are omitted so `@` stays
/// focused on files an agent can meaningfully work with.
pub(crate) fn workspace_locations(root: &Path) -> Vec<LocationEntry> {
    const MAX_LOCATIONS: usize = 5_000;
    workspace_locations_with_limit(root, MAX_LOCATIONS)
}

fn workspace_locations_with_limit(root: &Path, max_locations: usize) -> Vec<LocationEntry> {
    const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".next", "dist"];

    let mut entries = Vec::new();
    // Walk by depth so one large hidden subtree cannot starve its siblings.
    let mut pending = VecDeque::from([root.to_path_buf()]);
    while entries.len() < max_locations {
        let Some(dir) = pending.pop_front() else {
            break;
        };
        let Ok(children) = fs::read_dir(dir) else {
            continue;
        };
        let mut children: Vec<_> = children.filter_map(Result::ok).collect();
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            if entries.len() >= max_locations {
                break;
            }
            let Ok(kind) = child.file_type() else {
                continue;
            };
            let name = child.file_name();
            let name = name.to_string_lossy();
            if kind.is_dir() && SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            let path: PathBuf = child.path();
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            entries.push(LocationEntry {
                path: relative
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/"),
                directory: kind.is_dir(),
            });
            if kind.is_dir() {
                pending.push_back(path);
            }
        }
    }

    entries.sort_by(|left, right| {
        right
            .directory
            .cmp(&left.directory)
            .then_with(|| left.path.cmp(&right.path))
    });
    entries
}

#[cfg(test)]
mod tests {
    use super::workspace_locations_with_limit;

    #[test]
    fn leading_dot_folder_cannot_consume_the_whole_location_budget() {
        let root = std::env::temp_dir().join(format!(
            "orca-location-breadth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join(".first/one/two/three")).unwrap();
        std::fs::create_dir_all(root.join("z-last")).unwrap();
        std::fs::write(root.join("z-last/wanted.rs"), "fn wanted() {}").unwrap();

        let entries = workspace_locations_with_limit(&root, 4);

        assert!(
            entries.iter().any(|entry| entry.path == "z-last/wanted.rs"),
            "later workspace folders must remain searchable: {entries:?}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
