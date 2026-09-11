//! Host observation of a session's detached work, kept apart from parent
//! delivery: a host that reads these never consumes a completion the
//! parent transcript is owed, and a completion the transcript is not
//! owed (a workflow stage) is still observable here.

use orca_harness_tools::{CompletionInbox, SubagentNotification};
use tokio::sync::broadcast;

/// Capacity of a session's notification channel. A receiver that falls
/// further behind than this sees
/// [`RecvError::Lagged`](tokio::sync::broadcast::error::RecvError::Lagged)
/// and misses the oldest notifications: observation is lossy under
/// backpressure, parent delivery is not.
pub const NOTIFICATION_CAPACITY: usize = 256;

/// What a session reports about its detached subagents, read through
/// [`Session::notifications`](crate::Session::notifications). Nothing
/// here starts a model run: a host that wants the parent to react to
/// [`CompletionsReady`](Self::CompletionsReady) continues the session
/// itself.
#[derive(Clone, Debug)]
pub enum BackgroundNotification {
    /// A detached worker finished, however it ended. Sent for every
    /// worker, including cancelled ones (their `generation` is then
    /// stale) and workflow stages (`spawn.run.is_some()`), which never
    /// enter the parent transcript. Boxed: the payload carries the
    /// worker's full result, the other variants a few words.
    SubagentFinished(Box<SubagentNotification>),
    /// Results are waiting for the parent and no run is known to be on
    /// its way to deliver them. Sent at most once per idle period: the
    /// next run, whoever starts it, consumes the wake-up. A parent that
    /// was mid-run may have delivered the batch by the time a host reads
    /// this; check [`Session::pending_completions`] before continuing.
    ///
    /// [`Session::pending_completions`]: crate::Session::pending_completions
    CompletionsReady { pending: usize },
    /// A batch entered the parent transcript as one user turn, at a model
    /// boundary of a running turn or a host-started continuation.
    Delivered { spawn_ids: Vec<u64> },
}

/// The subagent notifier installed on a session's `subagent` tool: let
/// hosts observe every worker exit, admit the result for parent delivery
/// when the transcript is owed one, and flag the first idle-period
/// wake-up. Sending never fails the worker: a session with no receiver
/// simply drops the observation.
pub(crate) fn notify(
    inbox: &CompletionInbox,
    events: &broadcast::Sender<BackgroundNotification>,
    notification: SubagentNotification,
) {
    let _ = events.send(BackgroundNotification::SubagentFinished(Box::new(
        notification.clone(),
    )));
    if inbox.push(notification) && inbox.request_wakeup() {
        let _ = events.send(BackgroundNotification::CompletionsReady {
            pending: inbox.pending(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_harness_tools::{SubagentManager, SubagentSpawn};

    fn finished(generation: u64, id: u64, run: Option<u64>) -> SubagentNotification {
        SubagentNotification {
            generation,
            spawn: SubagentSpawn {
                id,
                parent_id: None,
                depth: 0,
                call_id: format!("call-{id}"),
                task: format!("task-{id}"),
                identity: None,
                run,
                stage: None,
            },
            result: Ok(serde_json::json!("done")),
        }
    }

    #[test]
    fn workflow_stages_are_observed_but_never_admitted_for_delivery() {
        let inbox = CompletionInbox::new(SubagentManager::default());
        let (events, mut host) = broadcast::channel(NOTIFICATION_CAPACITY);

        notify(&inbox, &events, finished(0, 7, Some(1)));
        assert!(matches!(
            host.try_recv().unwrap(),
            BackgroundNotification::SubagentFinished(n) if n.spawn.id == 7 && n.spawn.run == Some(1)
        ));
        assert!(host.try_recv().is_err(), "no wake-up for a stage");
        assert_eq!(inbox.pending(), 0);

        notify(&inbox, &events, finished(0, 8, None));
        assert!(matches!(
            host.try_recv().unwrap(),
            BackgroundNotification::SubagentFinished(n) if n.spawn.id == 8
        ));
        assert!(matches!(
            host.try_recv().unwrap(),
            BackgroundNotification::CompletionsReady { pending: 1 }
        ));
        assert_eq!(inbox.pending(), 1);
    }

    #[test]
    fn one_wakeup_per_idle_period_and_stale_generations_stay_out() {
        let inbox = CompletionInbox::new(SubagentManager::default());
        let (events, mut host) = broadcast::channel(NOTIFICATION_CAPACITY);

        notify(&inbox, &events, finished(0, 1, None));
        notify(&inbox, &events, finished(0, 2, None));
        let mut wakeups = 0;
        while let Ok(event) = host.try_recv() {
            if matches!(event, BackgroundNotification::CompletionsReady { .. }) {
                wakeups += 1;
            }
        }
        assert_eq!(wakeups, 1, "the second result joins the pending batch");
        assert_eq!(inbox.pending(), 2);

        inbox.reset();
        assert_eq!(inbox.pending(), 0);
        notify(&inbox, &events, finished(0, 3, None));
        assert!(matches!(
            host.try_recv().unwrap(),
            BackgroundNotification::SubagentFinished(n) if n.generation == 0
        ));
        assert!(
            host.try_recv().is_err(),
            "a stale result requests no wake-up"
        );
        assert_eq!(inbox.pending(), 0);
    }

    #[test]
    fn a_session_without_observers_still_delivers() {
        let inbox = CompletionInbox::new(SubagentManager::default());
        let (events, _) = broadcast::channel(NOTIFICATION_CAPACITY);
        notify(&inbox, &events, finished(0, 1, None));
        assert_eq!(inbox.pending(), 1);
    }
}
