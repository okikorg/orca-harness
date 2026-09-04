//! Release notes embedded at build time for the TUI changelog command.

pub(crate) static RELEASES: &[(&str, &str)] =
    include!(concat!(env!("OUT_DIR"), "/release_notes.rs"));

pub(crate) fn notes(full: bool) -> String {
    RELEASES
        .iter()
        .take(if full { RELEASES.len() } else { 1 })
        .map(|(version, notes)| {
            let title = if full {
                format!("## v{version}")
            } else {
                format!("# What's new in v{version}")
            };
            format!("{title}\n\n{}", changes_only(notes))
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

/// Release pages also carry installer instructions, asset tables, and a
/// compare link. Those are useful on GitHub but turn `/changelog` into a wall
/// of operational detail, so the TUI keeps only the actual release changes.
fn changes_only(notes: &str) -> String {
    let mut blocks = notes.split("\n\n").peekable();
    let mut kept = Vec::new();
    while let Some(block) = blocks.next() {
        let block = block.trim();
        if block.starts_with("## Install") {
            while blocks
                .peek()
                .is_some_and(|next| !next.trim().starts_with("## "))
            {
                blocks.next();
            }
            continue;
        }
        if block.starts_with("## Assets") {
            blocks.next(); // table
            if blocks.peek().is_some_and(|next| {
                let next = next.trim();
                next.starts_with("Verify downloads") || next.contains("SHA256SUMS")
            }) {
                blocks.next();
            }
            continue;
        }
        if !block.starts_with("**Full changelog:**") {
            kept.push(block);
        }
    }
    kept.join("\n\n").trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_is_one_release_and_full_is_newest_first() {
        let latest = notes(false);
        assert!(latest.starts_with("# What's new in v0.2.2"));
        assert!(!latest.contains("## v0.2.1"));
        assert!(!latest.contains("## Install"));
        assert!(!latest.contains("SHA256SUMS"));
        let full = notes(true);
        assert!(full.find("0.2.2").unwrap() < full.find("0.2.1").unwrap());
        assert!(full.contains("## v0.1.0"));
        assert!(!full.contains("## Assets"));
        assert!(!full.contains("curl -fsSL"));
        assert!(!full.contains("orcacode.ps1"));
        assert!(!full.contains("SHA256SUMS"));
        assert!(full.contains("With no API key configured"));
    }
}
