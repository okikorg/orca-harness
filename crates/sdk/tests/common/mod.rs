//! Shared fixtures for the SDK integration tests. Uses `std` only so that
//! `consumer_api.rs` keeps its "SDK plus third-party crates" import rule.

/// Creates a unique, empty directory under the system temp dir. Callers
/// remove it when they finish.
pub fn temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "orca-sdk-test-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
