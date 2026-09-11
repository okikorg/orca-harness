use orca_harness_core::Context;
use orca_harness_extensions::{workspace_key, SessionFile, SessionHandler};
use orca_harness_tools::Workspace;

use crate::{config, workspace_scope, Config};

/// Create a fresh session file, or resume one when --continue/--resume
/// asked. Errors are fatal at startup: recording (or the requested
/// resume) cannot happen, and silently running without it would lose
/// the transcript the user asked to keep.
pub(crate) fn open_session(cfg: &mut Config) -> Result<(SessionHandler, Option<Context>), String> {
    let base = config::sessions_dir().ok_or("no home directory for session storage")?;
    let scope = workspace_scope(&Workspace::new(&cfg.workspace));
    let dir = base.join(workspace_key(&scope));
    if cfg.continue_latest || cfg.resume_id.is_some() {
        let picked = match &cfg.resume_id {
            Some(id) => Some(find_session(&base, id)?),
            None => SessionFile::list(&dir).into_iter().next(),
        };
        let picked =
            picked.ok_or_else(|| format!("no session to resume under {}", dir.display()))?;
        let (handler, loaded) = SessionHandler::resume(&picked.path).map_err(|e| e.to_string())?;
        cfg.workspace = loaded.meta.workspace.into();
        for warning in &loaded.warnings {
            eprintln!("warning: {warning}");
        }
        Ok((handler, Some(loaded.context)))
    } else {
        let handler =
            SessionHandler::create(&dir, &scope, &cfg.model).map_err(|e| e.to_string())?;
        Ok((handler, None))
    }
}

/// Explicit IDs are global; prefixes must identify exactly one saved session.
fn find_session(base: &std::path::Path, id: &str) -> Result<SessionFile, String> {
    if id.is_empty() {
        return Err("session id must not be empty".into());
    }
    let entries = std::fs::read_dir(base).map_err(|e| e.to_string())?;
    let mut matches = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            matches.extend(
                SessionFile::list(&entry.path())
                    .into_iter()
                    .filter(|session| session.meta.id.starts_with(id)),
            );
        }
    }
    if let Some(index) = matches.iter().position(|session| session.meta.id == id) {
        return Ok(matches.swap_remove(index));
    }
    match matches.len() {
        0 => Err(format!("no session found for id: {id}")),
        1 => Ok(matches.remove(0)),
        _ => Err(format!("ambiguous session id: {id}; use a longer prefix")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_lookup_spans_workspaces_and_rejects_ambiguous_prefixes() {
        let base = std::env::temp_dir().join(format!("orca-resume-{}", std::process::id()));
        for (directory, workspace, id) in [
            ("first", "/first", "session-first"),
            ("second", "/second", "session-second"),
        ] {
            let dir = base.join(directory);
            let handler = SessionHandler::create(&dir, workspace, "model").unwrap();
            drop(handler);
            let mut session = SessionFile::list(&dir).remove(0);
            session.meta.id = id.into();
            std::fs::write(
                session.path,
                format!("{}\n", serde_json::to_string(&session.meta).unwrap()),
            )
            .unwrap();
        }
        let found = find_session(&base, "session-second").unwrap();
        assert_eq!(found.meta.workspace, "/second");
        let (_, loaded) = SessionHandler::resume(&found.path).unwrap();
        assert_eq!(loaded.meta.workspace, "/second");
        assert_eq!(
            find_session(&base, "session-s").unwrap().meta.id,
            "session-second"
        );
        assert!(find_session(&base, "missing").is_err());
        assert!(find_session(&base, "").is_err());
        assert!(find_session(&base, "session-")
            .unwrap_err()
            .contains("ambiguous"));
        std::fs::remove_dir_all(base).unwrap();
    }
}
