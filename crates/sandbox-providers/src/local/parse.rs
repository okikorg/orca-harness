//! Building `sh` command lines for the Docker adapter and reading back
//! what `stat` and `ls` print.

use orca_harness_core::{Entry, Stat};

/// Single-quote for `sh`, closing and reopening around embedded quotes.
pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// `stat -c '%.9Y %s %F'` — modification time as epoch seconds with a
/// nanosecond fraction, size, and a type description whose wording varies
/// ("directory", "regular file", "symbolic link"), so only the directory
/// case is matched by name. Whole seconds (`%Y`, or a `stat` without the
/// precision flag) still parse; they are just coarser.
pub(crate) fn parse_stat(line: &str) -> Option<Stat> {
    let mut parts = line.trim().splitn(3, ' ');
    let modified = parse_epoch(parts.next()?)?;
    let len = parts.next()?.parse::<u64>().ok()?;
    let is_dir = parts.next().is_some_and(|kind| kind.trim() == "directory");
    Some(Stat {
        modified: Some(modified),
        len,
        is_dir,
    })
}

/// `seconds[.fraction]`, keeping up to nine fractional digits exactly
/// rather than going through a float.
fn parse_epoch(text: &str) -> Option<std::time::SystemTime> {
    let (secs, fraction) = text.split_once('.').unwrap_or((text, ""));
    let secs = secs.parse::<u64>().ok()?;
    if fraction.len() > 9 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let nanos = format!("{fraction:0<9}").parse::<u32>().ok()?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::new(secs, nanos))
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
        let stat = parse_stat("1757635200.123456789 4096 regular file\n").expect("parse");
        assert_eq!(
            stat.modified,
            Some(std::time::UNIX_EPOCH + std::time::Duration::new(1757635200, 123456789))
        );
        assert_eq!(stat.len, 4096);
        assert!(!stat.is_dir);

        let dir = parse_stat("1757635200 64 directory").expect("whole seconds still parse");
        assert!(dir.is_dir);
        assert_eq!(
            dir.modified,
            Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1757635200))
        );

        // Garbage must not become a stamp that silently compares equal.
        assert!(parse_stat("").is_none());
        assert!(parse_stat("nonsense").is_none());
        assert!(parse_stat("1757635200.1x 4 regular file").is_none());
        assert!(parse_stat("1757635200.1234567890 4 regular file").is_none());
    }

    #[test]
    fn edits_within_one_second_get_different_stamps() {
        // The guard compares mtime and length; two same-length writes in
        // the same second must still differ.
        let first = parse_stat("1757635200.100000000 5 regular file").unwrap();
        let second = parse_stat("1757635200.500000000 5 regular file").unwrap();
        assert_ne!(first, second);
        assert_eq!(
            parse_stat("1757635200.5 5 regular file").unwrap(),
            second,
            "a short fraction is tenths, not nanoseconds"
        );
    }
}
