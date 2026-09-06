use super::*;

pub(super) enum After {
    Nothing,
    Close,
    /// Close a catalog overlay and place its selected command in the composer.
    CloseAndCompose(String),
    /// Replace the active picker while retaining it as a left-arrow parent.
    Push(Overlay),
    /// Change the active editor while retaining its existing parent.
    Replace(Overlay),
    /// Send to the worker with the overlay left open (toggle rows).
    Send(WorkerCmd),
    CloseAndSend(WorkerCmd),
    /// Close, remember the picked model's context window, switch model.
    CloseAndSetModel {
        id: String,
        window: Option<u64>,
        reasoning_effort: Option<String>,
    },
    /// Close, persist, and activate the selected transcript layout.
    CloseAndSetView(ViewMode),
    /// Close and activate the picked session mode on the shared
    /// handle the gates read per tool call.
    CloseAndSetMode(crate::mode::Mode),
    /// Close, persist, and activate transcript section spacing.
    CloseAndSetTranscriptSpacing(TranscriptSpacing),
    CloseAndSetStyle(UiStyle),
    /// Close, persist, and activate split inspector rendering.
    CloseAndSetInspector(InspectorMode),
    /// Close and drop a dim status line into the history.
    CloseWithNote(String),
    /// Return to the parent picker and drop a dim status line.
    PopWithNote(String),
    /// Keep the overlay open and drop a dim status line (row
    /// actions that mutate the list in place).
    Note(String),
    /// Close, send to the worker, and drop a dim status line.
    SendWithNote(WorkerCmd, String),
    /// Close and start the /models fetch-then-pick flow.
    FetchModels,
    FetchSubagentModels {
        tier: String,
        provider: crate::Provider,
    },
    /// Delete an installed skill, then rescan. Deferred out of the
    /// overlay match because it needs `app` (the shared handle, the
    /// config, the transcript), which the match holds borrowed.
    RemoveSkill(String),
    /// Apply a registered-plugin row action after the overlay borrow ends.
    PluginAction {
        plugin: crate::config::RegisteredPlugin,
        action: char,
    },
    /// Close and replace the active `@query` with a workspace path.
    InsertLocation {
        token_start: usize,
        entry: LocationEntry,
    },
    /// Close and replace the active `$query` with an invokable skill.
    InsertSkill {
        token_start: usize,
        name: String,
    },
}
