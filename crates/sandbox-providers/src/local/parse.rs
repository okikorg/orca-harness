//! Building `sh` command lines for the Docker adapter and reading back
//! what `stat` and `ls` print.

use orca_harness_core::{Entry, Stat};

/// Single-quote for `sh`, closing and reopening around embedded quotes.
pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// `stat -c '%Y %s %F'` — epoch seconds, size, and a type description
/// whose wording varies ("directory", "regular file", "symbolic link"),
/// so only the directory case is matched by name.
pub(crate) fn parse_stat(line: &str) -> Option<Stat> {
    let mut parts = line.trim().splitn(3, ' ');
    let modified = parts
        .next()?
        .parse::<u64>()
        .ok()
        .map(|secs| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs));
    let len = parts.next()?.parse::<u64>().ok()?;
    let is_dir = parts.next().is_some_and(|kind| kind.trim() == "directory");
    Some(Stat {
        modified,
        len,
        is_dir,
    })
}

/// `ls -Ap` marks directories with a trailing slash and omits `.`/`..`.
pub(crate) fn parse_ls(listing: &str) -> Vec<Entry> {
    listing
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(|line| match line.strip_suffix('/') {
            Some(name) => Entry {
                name: name.to_string(),
                is_dir: true,
            },
            None => Entry {
                name: line.to_string(),
                is_dir: false,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_embedded_quotes_and_spaces() {
        assert_eq!(shell_quote("/workspace/a b"), "'/workspace/a b'");
        // The classic break: a single quote inside the value must close,
        // escape, and reopen rather than terminate the argument early.
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("a;rm -rf /"), "'a;rm -rf /'");
    }

    #[test]
    fn ls_listing_separates_directories_from_files() {
        let entries = parse_ls("src/\nCargo.toml\ntarget/\n\n");
        assert_eq!(
            entries,
            vec![
                Entry {
                    name: "src".into(),
                    is_dir: true
                },
                Entry {
                    name: "Cargo.toml".into(),
                    is_dir: false
                },
                Entry {
                    name: "target".into(),
                    is_dir: true
                },
            ]
        );
    }

    #[test]
    fn stat_output_parses_into_a_comparable_stamp() {
        let stat = parse_stat("1757635200 4096 regular file\n").expect("parse");
        assert_eq!(
            stat.modified,
            Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1757635200))
        );
        assert_eq!(stat.len, 4096);
        assert!(!stat.is_dir);

        let dir = parse_stat("1757635200 64 directory").expect("parse");
        assert!(dir.is_dir);

        // Garbage must not become a stamp that silently compares equal.
        assert!(parse_stat("").is_none());
        assert!(parse_stat("nonsense").is_none());
    }
}
