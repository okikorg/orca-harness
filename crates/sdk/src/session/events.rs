//! Fan-out of a run's lifecycle events to the request's synchronous
//! callback and to the bounded observation channel behind
//! [`RunHandle::events`](crate::RunHandle::events).
//!
//! The channel is observational: the run never waits for a reader. When
//! the channel is full the event is counted as dropped, and the next event
//! that fits is preceded by one [`RunEvent::Overflow`] marker carrying the
//! size of the gap. A gap still open when the run ends is reported by a
//! final marker sent through a permit reserved at channel creation, so
//! every dropped event is accounted for in the stream even when nobody
//! reads it until the run is over. A closed channel (the reader was
//! dropped) is not a loss: nothing is counted and sending stops.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use orca_harness_extensions::EventSink;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{self, OwnedPermit, Receiver, Sender};

use crate::{EventCallback, HarnessEvent, RunEvent};

/// The sending half of a run's observation channel, one permit held back
/// for the final overflow marker, and the shared dropped-event counter
/// the handle reads.
pub(super) struct RunObserver {
    sender: Sender<RunEvent>,
    reserved: OwnedPermit<RunEvent>,
    dropped: Arc<AtomicU64>,
}

impl RunObserver {
    /// A channel holding `capacity` events for the reader plus the one
    /// reserved slot. `capacity` is at least one
    /// ([`RunRequest::event_capacity`](crate::RunRequest::event_capacity)
    /// clamps it).
    pub(super) fn channel(capacity: usize) -> (Self, Receiver<RunEvent>, Arc<AtomicU64>) {
        let (sender, receiver) = mpsc::channel(capacity + 1);
        let reserved = sender
            .clone()
            .try_reserve_owned()
            .expect("a fresh channel has a free slot");
        let dropped = Arc::new(AtomicU64::new(0));
        let observer = Self {
            sender,
            reserved,
            dropped: dropped.clone(),
        };
        (observer, receiver, dropped)
    }
}

pub(super) struct EventFanout {
    callback: Option<EventCallback>,
    sender: Option<Sender<RunEvent>>,
    reserved: Mutex<Option<OwnedPermit<RunEvent>>>,
    dropped: Arc<AtomicU64>,
    /// Events dropped since the last delivered marker; reported by the
    /// next one.
    pending_gap: AtomicU64,
}

impl EventFanout {
    pub(super) fn new(callback: Option<EventCallback>, observer: Option<RunObserver>) -> Self {
        let (sender, reserved, dropped) = match observer {
            Some(observer) => (
                Some(observer.sender),
                Some(observer.reserved),
                observer.dropped,
            ),
            None => (None, None, Arc::new(AtomicU64::new(0))),
        };
        Self {
            callback,
            sender,
            reserved: Mutex::new(reserved),
            dropped,
            pending_gap: AtomicU64::new(0),
        }
    }

    /// Report a gap left open by the last events of the run through the
    /// reserved slot, and return the total dropped. Call once, after the
    /// kernel has finished emitting.
    pub(super) fn flush(&self) -> u64 {
        let reserved = self.reserved.lock().unwrap().take();
        let gap = self.pending_gap.swap(0, Ordering::AcqRel);
        if let Some(permit) = reserved {
            if gap > 0 {
                permit.send(RunEvent::Overflow { dropped: gap });
            }
            // Otherwise dropping the permit releases the slot.
        }
        self.dropped.load(Ordering::Acquire)
    }

    fn record_drop(&self, gap: u64) {
        self.dropped.fetch_add(1, Ordering::AcqRel);
        self.pending_gap.fetch_add(gap + 1, Ordering::AcqRel);
    }

    /// Offer one event to the channel without waiting. Never blocks: a
    /// full channel drops the event, a closed one ends observation.
    fn offer(&self, sender: &Sender<RunEvent>, event: HarnessEvent) {
        let gap = self.pending_gap.swap(0, Ordering::AcqRel);
        if gap > 0 {
            match sender.try_send(RunEvent::Overflow { dropped: gap }) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => return self.record_drop(gap),
                Err(TrySendError::Closed(_)) => return,
            }
        }
        if let Err(TrySendError::Full(_)) = sender.try_send(RunEvent::Harness(event)) {
            self.record_drop(0);
        }
    }
}

#[async_trait]
impl EventSink for EventFanout {
    // No `.await` here: the kernel calls this on its hot path and a slow
    // or absent reader must never stall the run.
    async fn emit(&self, event: HarnessEvent) {
        match (&self.callback, &self.sender) {
            (Some(callback), Some(sender)) => {
                callback(event.clone());
                self.offer(sender, event);
            }
            (Some(callback), None) => callback(event),
            (None, Some(sender)) => self.offer(sender, event),
            (None, None) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> HarnessEvent {
        HarnessEvent::AgentStart
    }

    fn fanout(capacity: usize) -> (EventFanout, Receiver<RunEvent>, Arc<AtomicU64>) {
        let (observer, receiver, dropped) = RunObserver::channel(capacity);
        (EventFanout::new(None, Some(observer)), receiver, dropped)
    }

    #[tokio::test]
    async fn full_channel_drops_and_marks_the_gap_once() {
        let (fanout, mut receiver, dropped) = fanout(2);
        for _ in 0..5 {
            fanout.emit(event()).await;
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 3);
        assert!(matches!(receiver.recv().await, Some(RunEvent::Harness(_))));
        assert!(matches!(receiver.recv().await, Some(RunEvent::Harness(_))));

        // Two slots free: the marker and the next event both fit.
        fanout.emit(event()).await;
        assert!(matches!(
            receiver.recv().await,
            Some(RunEvent::Overflow { dropped: 3 })
        ));
        assert!(matches!(receiver.recv().await, Some(RunEvent::Harness(_))));
        assert_eq!(fanout.flush(), 3);
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn marker_fits_but_event_does_not_opens_a_new_gap() {
        let (fanout, mut receiver, dropped) = fanout(1);
        fanout.emit(event()).await;
        fanout.emit(event()).await; // dropped: gap 1
        assert!(matches!(receiver.recv().await, Some(RunEvent::Harness(_))));
        fanout.emit(event()).await; // marker fits, event does not: gap 1 again
        assert!(matches!(
            receiver.recv().await,
            Some(RunEvent::Overflow { dropped: 1 })
        ));
        fanout.emit(event()).await; // marker fits, event does not
        assert!(matches!(
            receiver.recv().await,
            Some(RunEvent::Overflow { dropped: 1 })
        ));
        assert_eq!(dropped.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn marker_that_does_not_fit_keeps_the_gap_pending() {
        let (fanout, mut receiver, dropped) = fanout(1);
        fanout.emit(event()).await;
        fanout.emit(event()).await; // dropped: gap 1
        fanout.emit(event()).await; // marker does not fit: gap 2
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
        assert!(matches!(receiver.recv().await, Some(RunEvent::Harness(_))));
        fanout.emit(event()).await;
        assert!(matches!(
            receiver.recv().await,
            Some(RunEvent::Overflow { dropped: 2 })
        ));
    }

    #[tokio::test]
    async fn trailing_gap_is_flushed_through_the_reserved_slot() {
        let (fanout, mut receiver, _dropped) = fanout(1);
        for _ in 0..4 {
            fanout.emit(event()).await;
        }
        assert_eq!(fanout.flush(), 3);
        assert!(matches!(receiver.recv().await, Some(RunEvent::Harness(_))));
        assert!(matches!(
            receiver.recv().await,
            Some(RunEvent::Overflow { dropped: 3 })
        ));
        drop(fanout);
        assert!(receiver.recv().await.is_none());
    }

    #[tokio::test]
    async fn closed_channel_is_not_counted_as_loss() {
        let (fanout, receiver, dropped) = fanout(1);
        drop(receiver);
        for _ in 0..3 {
            fanout.emit(event()).await;
        }
        assert_eq!(fanout.flush(), 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
    }
}
