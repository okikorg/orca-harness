//! Release notes embedded at build time for the TUI changelog command.

pub(crate) static RELEASES: &[(&str, &str)] =
    include!(concat!(env!("OUT_DIR"), "/release_notes.rs"));

pub(crate) fn notes(full: bool) -> String {
    RELEASES
        .iter()
        .take(if full { RELEASES.len() } else { 1 })
        .map(|(version, notes)| format!("# orcacode {version}\n\n{}", notes.trim()))
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_is_one_release_and_full_is_newest_first() {
        let latest = notes(false);
        assert!(latest.starts_with("# orcacode 0.2.2"));
        assert!(!latest.contains("# orcacode 0.2.1"));
        let full = notes(true);
        assert!(full.find("0.2.2").unwrap() < full.find("0.2.1").unwrap());
        assert!(full.contains("# orcacode 0.1.0"));
    }
}
