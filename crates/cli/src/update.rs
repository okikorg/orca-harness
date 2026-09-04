//! Self-update through the same public, checksum-verifying installer used for fresh installs.

use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::mpsc;

use crate::msg::UiMsg;

const LATEST_URL: &str = "https://orcapods.ai/dl/orcacode/latest.json";

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

pub(crate) fn check_in_background(ui: mpsc::UnboundedSender<UiMsg>) {
    tokio::spawn(async move {
        let result = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .ok()?
            .get(LATEST_URL)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json::<Latest>()
            .await
            .ok()?;
        let current = env!("CARGO_PKG_VERSION");
        if is_newer(&result.version, current) {
            let _ = ui.send(UiMsg::Notice(format!(
                "update available · {current} → {} · run `orcacode update`",
                result.version
            )));
        }
        Some(())
    });
}

#[derive(Deserialize)]
struct Latest {
    version: String,
}

fn is_newer(latest: &str, current: &str) -> bool {
    version_parts(latest) > version_parts(current)
}

fn version_parts(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.split_once('-').map_or(version, |(core, _)| core);
    let mut parts = core.split('.').map(str::parse::<u64>);
    Some((
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    ))
    .filter(|_| parts.next().is_none())
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

    #[test]
    fn update_comparison_is_numeric_and_fails_closed() {
        assert!(is_newer("0.10.0", "0.2.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.2.2", "0.2.2"));
        assert!(!is_newer("0.2.1", "0.2.2"));
        assert!(!is_newer("invalid", "0.2.2"));
    }
}
