//! Host observation of a session's detached work, kept apart from parent
//! delivery: a host that reads these never consumes a completion the
//! parent transcript is owed, and a completion the transcript is not
//! owed (a workflow stage) is still observable here. Background
//! processes report on the same channel, separate from run events: a
//! process outlives the run that started it.

use orca_harness_tools::{CompletionInbox, ProcessNotification, SubagentNotification};
use tokio::sync::broadcast;

/// Capacity of a session's notification channel. A receiver that falls
/// further behind than this sees
/// [`RecvError::Lagged`](tokio::sync::broadcast::error::RecvError::Lagged)
/// and misses the oldest notifications: observation is lossy under
/// backpressure, parent delivery is not.
pub const NOTIFICATION_CAPACITY: usize = 256;

/// What a session reports about its detached subagents and background
/// processes, read through
/// [`Session::notifications`](crate::Session::notifications). Nothing
/// here starts a model run: a host that wants the parent to react to
/// [`CompletionsReady`](Self::CompletionsReady) continues the session
/// itself.
///
/// # Ordering
///
/// [`SubagentFinished`](Self::SubagentFinished) is broadcast before the
/// result is pushed into the inbox, so a
/// [`Session::pending_completions`](crate::Session::pending_completions)
/// read right after observing it may not count that result yet; wait for
/// [`CompletionsReady`](Self::CompletionsReady) before relying on the
/// count. `CompletionsReady` follows the first admission made while no
/// wake-up is outstanding, so one `CompletionsReady` can stand for several
/// `SubagentFinished`. A workflow records its
/// [`Workflows::status`](crate::Workflows::status) `outcome` before its
/// run-level `SubagentFinished` is sent, so a host that observes the run
/// finish can read the outcome at once.
#[derive(Clone, Debug)]
pub enum BackgroundNotification {
    /// A detached worker finished, however it ended. Sent for every
    /// worker, including cancelled ones (their `generation` is then
    /// stale) and workflow stages (`spawn.run.is_some()`), which never
    /// enter the parent transcript. Boxed: the payload carries the
    /// worker's full result, the other variants a few words.
    SubagentFinished(Box<SubagentNotification>),
    /// Results are waiting for the parent. Sent when a result is admitted
    /// while no wake-up is outstanding, and again by a run that ends with
    /// results still waiting. A running turn may deliver the batch before
    /// a host reads this (a [`CompletionsDelivered`] follows); check
    /// [`Session::pending_completions`] before continuing.
    ///
    /// [`CompletionsDelivered`]: Self::CompletionsDelivered
    /// [`Session::pending_completions`]: crate::Session::pending_completions
    CompletionsReady { pending: usize },
    /// A batch entered the parent transcript as one user turn, at a model
    /// boundary of a running turn or a host-started continuation.
    CompletionsDelivered { spawn_ids: Vec<u64> },
    /// A detached background process (host- or model-started through
    /// the session's `process` tool) exited, or its output first matched
    /// the pattern its spawn asked to be told about. Carries the output
    /// that accrued since the last drain. Not sent for processes the
    /// host or the model killed, nor for those [`Session::clear`]
    /// killed; a process [`Session::shutdown`] kills may still report
    /// its exit.
    ///
    /// [`Session::clear`]: crate::Session::clear
    /// [`Session::shutdown`]: crate::Session::shutdown
    ProcessNotified(ProcessNotification),
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

/// Called when a run ends. A result admitted after the run's last model
/// boundary found the wake-up still outstanding from a result that run
/// delivered, so it announced nothing; the run's own wake-up consumption
/// would then discard it. Clear the latch and, when results are still
/// waiting, announce them: exactly one of the worker and the run sees
/// the latch clear, so each waiting batch is announced once.
pub(crate) fn rearm(inbox: &CompletionInbox, events: &broadcast::Sender<BackgroundNotification>) {
    if inbox.consume_wakeup() && inbox.request_wakeup() {
        let _ = events.send(BackgroundNotification::CompletionsReady {
            pending: inbox.pending(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_harness_tools::{SubagentManager, SubagentSpawn};

    fn ready_count(host: &mut broadcast::Receiver<BackgroundNotification>) -> usize {
        let mut count = 0;
        while let Ok(event) = host.try_recv() {
            if matches!(event, BackgroundNotification::CompletionsReady { .. }) {
                count += 1;
            }
        }
        count
    }

    #[test]
    fn a_run_ending_with_waiting_results_announces_them_once() {
        let inbox = CompletionInbox::new(SubagentManager::default());
        let (events, mut host) = broadcast::channel(NOTIFICATION_CAPACITY);

        // Run starts; A finishes mid-run and is delivered at a model boundary.
        inbox.consume_wakeup();
        notify(&inbox, &events, finished(0, 1, None));
        assert_eq!(ready_count(&mut host), 1);
        assert_eq!(inbox.drain().len(), 1);
        // B finishes after the last boundary: the latch is still set.
        notify(&inbox, &events, finished(0, 2, None));
        assert_eq!(ready_count(&mut host), 0, "B found the wake-up outstanding");
        assert_eq!(inbox.pending(), 1);

        rearm(&inbox, &events);
        assert_eq!(ready_count(&mut host), 1, "the run announces B");
        rearm(&inbox, &events);
        assert_eq!(
            ready_count(&mut host),
            1,
            "a second rearm is silent, B is still latched"
        );

        // A run that ends with nothing waiting clears the latch silently,
        // so the next idle result announces itself.
        assert_eq!(inbox.drain().len(), 1);
        rearm(&inbox, &events);
        assert_eq!(ready_count(&mut host), 0);
        notify(&inbox, &events, finished(0, 3, None));
        assert_eq!(ready_count(&mut host), 1);
    }

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
        assert_eq!(
            ready_count(&mut host),
            1,
            "the second result joins the pending batch"
        );
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
