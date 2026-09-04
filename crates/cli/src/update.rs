//! Self-update through the same public, checksum-verifying installer used for fresh installs.

use std::process::{Command, Stdio};

#[cfg(unix)]
const PROGRAM: &str = "sh";
#[cfg(unix)]
const ARGS: &[&str] = &[
    "-c",
    "tmp=$(mktemp); trap 'rm -f \"$tmp\"' EXIT; curl -fsSL https://orcapods.ai/orcacode.sh -o \"$tmp\" && sh \"$tmp\"",
];

#[cfg(windows)]
const PROGRAM: &str = "powershell";
#[cfg(windows)]
const ARGS: &[&str] = &[
    "-NoProfile",
    "-Command",
    "$ErrorActionPreference = 'Stop'; irm https://orcapods.ai/orcacode.ps1 | iex",
];

pub(crate) fn run() -> Result<(), String> {
    println!(
        "Updating orcacode {} to the latest release...",
        env!("CARGO_PKG_VERSION")
    );
    let status = Command::new(PROGRAM)
        .args(ARGS)
        .stdin(Stdio::null())
        .status()
        .map_err(|error| format!("could not start the installer: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("installer exited with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updater_uses_the_public_installer_and_propagates_download_failures() {
        let command = ARGS.join(" ");
        assert!(command.contains("https://orcapods.ai/orcacode."));
        #[cfg(unix)]
        assert!(command.contains("curl -fsSL"));
        #[cfg(windows)]
        assert!(command.contains("ErrorActionPreference = 'Stop'"));
    }
}
