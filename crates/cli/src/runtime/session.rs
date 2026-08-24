use orca_harness_core::Context;
use orca_harness_extensions::{workspace_key, SessionFile, SessionHandler};
use orca_harness_tools::Workspace;

use crate::{config, workspace_scope, Config};

/// Create a fresh session file, or resume one when --continue/--resume
/// asked. Errors are fatal at startup: recording (or the requested
/// resume) cannot happen, and silently running without it would lose
/// the transcript the user asked to keep.
pub(crate) fn open_session(
    cfg: &Config,
    ws: &Workspace,
) -> Result<(SessionHandler, Option<Context>), String> {
    let base = config::sessions_dir().ok_or("no home directory for session storage")?;
    let scope = workspace_scope(ws);
    let dir = base.join(workspace_key(&scope));
    if cfg.continue_latest || cfg.resume_id.is_some() {
        let sessions = SessionFile::list(&dir);
        let picked = match &cfg.resume_id {
            Some(id) => sessions
                .into_iter()
                .find(|s| s.meta.id.starts_with(id.as_str())),
            None => sessions.into_iter().next(),
        };
        let picked =
            picked.ok_or_else(|| format!("no session to resume under {}", dir.display()))?;
        let (handler, loaded) = SessionHandler::resume(&picked.path).map_err(|e| e.to_string())?;
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
