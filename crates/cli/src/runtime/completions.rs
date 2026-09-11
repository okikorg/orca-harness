//! The interactive host's view of detached-subagent completions.
//!
//! Bookkeeping lives in the tools crate ([`CompletionInbox`] and
//! [`CompletionDelivery`]); this module adds what only this host knows:
//! the transcript pane that renders each completion, the notice that says a
//! batch reached the parent, and the worker command that starts a hidden
//! wake-up run when the parent is idle.

use tokio::sync::mpsc;

use orca_harness_tools::{CompletionDelivery, CompletionInbox, SubagentNotification};

use crate::msg::{UiMsg, WorkerCmd};

/// Route one completion to the host and, when owed, to the parent.
pub(crate) trait PublishCompletion {
    /// Terminal UI state is independent of parent delivery: action=cancel_all
    /// invalidates results but still must settle existing visible workers,
    /// and a workflow stage settles its pane without ever reaching the parent.
    fn publish(
        &self,
        notification: SubagentNotification,
        ui: &mpsc::UnboundedSender<UiMsg>,
        worker: &mpsc::UnboundedSender<WorkerCmd>,
    );
}

impl PublishCompletion for CompletionInbox {
    fn publish(
        &self,
        notification: SubagentNotification,
        ui: &mpsc::UnboundedSender<UiMsg>,
        worker: &mpsc::UnboundedSender<WorkerCmd>,
    ) {
        let _ = ui.send(UiMsg::SubagentCompleted {
            id: notification.spawn.id,
            is_error: notification.result.is_err(),
            message: match &notification.result {
                Ok(result) => result["answer"].as_str().unwrap_or_default().to_owned(),
                Err(error) => error.clone(),
            },
        });
        if self.push(notification) && self.request_wakeup() {
            let _ = worker.send(WorkerCmd::BackgroundSubagentsReady);
        }
    }
}

/// Delivery that also tells the transcript which agents just reported.
pub(crate) fn delivery(
    inbox: CompletionInbox,
    ui: mpsc::UnboundedSender<UiMsg>,
) -> CompletionDelivery {
    CompletionDelivery::new(inbox).on_delivered(move |batch| {
        let _ = ui.send(UiMsg::Notice(delivery_notice(batch)));
    })
}

/// One transcript line saying which agents' results the parent just got.
pub(crate) fn delivery_notice(batch: &[SubagentNotification]) -> String {
    let ids = batch
        .iter()
        .map(|notification| notification.spawn.id.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let noun = if batch.len() == 1 {
        "background agent"
    } else {
        "background agents"
    };
    format!("{} {noun} reported · spawn {ids}", batch.len())
}

#[cfg(test)]
mod tests;
