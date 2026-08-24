use super::*;

pub(super) enum After {
    Nothing,
    Close,
    Replace(Overlay),
    /// Send to the worker with the overlay left open (toggle rows).
    Send(WorkerCmd),
    CloseAndSend(WorkerCmd),
    /// Close, remember the picked model's context window, switch model.
    CloseAndSetModel {
        id: String,
        window: Option<u64>,
    },
    /// Close, persist, and activate the selected transcript layout.
    CloseAndSetView(ViewMode),
    /// Close, persist, and activate transcript section spacing.
    CloseAndSetTranscriptSpacing(TranscriptSpacing),
    /// Close and drop a dim status line into the history.
    CloseWithNote(String),
    /// Keep the overlay open and drop a dim status line (row
    /// actions that mutate the list in place).
    Note(String),
    /// Close, send to the worker, and drop a dim status line.
    SendWithNote(WorkerCmd, String),
    /// Close and start the /models fetch-then-pick flow.
    FetchModels,
    /// Delete an installed skill, then rescan. Deferred out of the
    /// overlay match because it needs `app` (the shared handle, the
    /// config, the transcript), which the match holds borrowed.
    RemoveSkill(String),
    /// Close and replace the active `@query` with a workspace path.
    InsertLocation {
        token_start: usize,
        entry: LocationEntry,
    },
}
