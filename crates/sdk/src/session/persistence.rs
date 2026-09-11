use std::path::{Path, PathBuf};

use orca_harness_extensions::TruncationStore;

use crate::{Agent, SdkError};

pub(super) fn session_store(agent: &Agent) -> TruncationStore {
    TruncationStore::new(session_store_budget(agent))
}

pub(super) fn load_session_store(
    agent: &Agent,
    session_path: Option<&Path>,
) -> Result<TruncationStore, SdkError> {
    match session_path {
        Some(path) => Ok(TruncationStore::load(
            &recovery_path(path),
            session_store_budget(agent),
        )?),
        None => Ok(session_store(agent)),
    }
}

fn session_store_budget(agent: &Agent) -> usize {
    agent
        .inner
        .extension_config
        .truncation
        .map(|config| config.store_budget_bytes)
        .unwrap_or(16 * 1024 * 1024)
}

fn recovery_path(session_path: &Path) -> PathBuf {
    session_path.with_extension("recovery.json")
}

pub(super) fn save_session_store(
    store: &TruncationStore,
    session_path: &Path,
) -> Result<(), SdkError> {
    store.save(&recovery_path(session_path))?;
    Ok(())
}
