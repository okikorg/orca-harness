//! Opening sessions: the on-disk session listing, the ephemeral or
//! persistent mode, and the builder that seeds a session's conversation.

use std::path::{Path, PathBuf};

use orca_harness_core::Message;
use orca_harness_extensions::SessionFile;

use super::Session;
use crate::{Agent, SdkError};

#[derive(Clone)]
pub struct Sessions {
    dir: PathBuf,
}

impl Sessions {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn list(&self) -> Vec<SessionFile> {
        SessionFile::list(&self.dir)
    }

    pub fn directory(&self) -> &Path {
        &self.dir
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SessionMode {
    #[default]
    Ephemeral,
    Persistent,
}

pub struct SessionBuilder {
    agent: Agent,
    mode: SessionMode,
    context: Vec<Message>,
}

impl SessionBuilder {
    pub(crate) fn new(agent: Agent) -> Self {
        Self {
            agent,
            mode: SessionMode::Ephemeral,
            context: Vec::new(),
        }
    }

    /// Seed the session with an existing conversation instead of an empty
    /// one. A persistent session writes the imported history to disk when
    /// it opens, so a later `resume_session` sees it.
    ///
    /// System-prompt precedence: the agent's system prompt is
    /// authoritative. When the agent has one, it replaces a leading
    /// imported `System` message, or is prepended when the import has
    /// none. When the agent has no system prompt, an imported leading
    /// `System` message is kept as-is.
    ///
    /// [`open`](Self::open) rejects a history the kernel cannot resume from
    /// with [`SdkError::InvalidContext`]: more than one `System` message or
    /// one that is not first; a `Tool` message that does not immediately
    /// follow an `Assistant` message whose tool calls it answers (the
    /// result ids must match the call ids exactly); or an `Assistant`
    /// message with tool calls that is not immediately followed by its
    /// `Tool` message. An empty import is the same as none.
    pub fn context(mut self, messages: impl IntoIterator<Item = Message>) -> Self {
        self.context = messages.into_iter().collect();
        self
    }

    pub fn ephemeral(mut self) -> Self {
        self.mode = SessionMode::Ephemeral;
        self
    }

    pub fn persistent(mut self) -> Self {
        self.mode = SessionMode::Persistent;
        self
    }

    pub fn open(self) -> Result<Session, SdkError> {
        Session::open(self.agent, self.mode, self.context)
    }
}
