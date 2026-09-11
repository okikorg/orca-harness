use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::{CancellationToken, Image, Message, Usage};
use orca_harness_extensions::HarnessEvent;
use tokio::sync::mpsc;

use crate::SdkError;

pub type EventCallback = Arc<dyn Fn(HarnessEvent) + Send + Sync>;

/// Default capacity of the observation channel behind
/// [`RunHandle::events`]; see [`RunRequest::event_capacity`].
pub const DEFAULT_EVENT_CAPACITY: usize = 1024;

#[derive(Clone)]
pub struct RunRequest {
    pub prompt: String,
    pub images: Vec<Image>,
    pub deadline: Option<Duration>,
    pub(crate) on_event: Option<EventCallback>,
    pub(crate) continue_at_step_limit: bool,
    /// Built by [`RunRequest::continuation`]: no user message is appended.
    pub(crate) continuation: bool,
    /// A caller-owned token; the run gets a child of it.
    pub(crate) cancellation: Option<CancellationToken>,
    pub(crate) event_capacity: usize,
}

impl RunRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            images: Vec::new(),
            deadline: None,
            on_event: None,
            continue_at_step_limit: false,
            continuation: false,
            cancellation: None,
            event_capacity: DEFAULT_EVENT_CAPACITY,
        }
    }

    /// A request that continues the session's conversation from where it
    /// stands, without appending a user message. The model sees the
    /// transcript as-is (its last message is typically an earlier
    /// assistant turn) and produces the next assistant turn.
    ///
    /// Limits, [`deadline`](Self::deadline), [`on_event`](Self::on_event),
    /// [`continue_at_step_limit`](Self::continue_at_step_limit), and
    /// [`cancellation`](Self::cancellation) apply as on any request.
    /// A prompt or images cannot be attached (there is no user message to
    /// carry them); either is rejected with [`SdkError::Config`] when the
    /// run starts. A session whose transcript holds nothing beyond the
    /// system prompt rejects the request with [`SdkError::InvalidContext`].
    ///
    /// ```rust,no_run
    /// # async fn example(session: orca_harness_sdk::Session) -> Result<(), orca_harness_sdk::SdkError> {
    /// use orca_harness_sdk::RunRequest;
    /// let result = session.continue_run(RunRequest::continuation()).await?;
    /// println!("{}", result.text);
    /// # Ok(())
    /// # }
    /// ```
    pub fn continuation() -> Self {
        Self {
            continuation: true,
            ..Self::new(String::new())
        }
    }

    /// Tie the run to a caller-owned token. The run receives a child of
    /// `token`: cancelling `token` cancels the run, while cancelling the
    /// run (through [`RunHandle::cancellation_token`] or by dropping the
    /// handle) never cancels `token`, so one parent token can govern many
    /// runs and other work without them cancelling each other.
    pub fn cancellation(mut self, token: CancellationToken) -> Self {
        self.cancellation = Some(token);
        self
    }

    /// The token this run observes: a child of the caller's token when one
    /// was supplied, otherwise a fresh root.
    pub(crate) fn run_token(&self) -> CancellationToken {
        match &self.cancellation {
            Some(parent) => parent.child_token(),
            None => CancellationToken::new(),
        }
    }

    /// Continue the same conversation after each bounded kernel run reaches its
    /// model-step limit. Disabled by default. Cancellation and the absolute
    /// deadline still apply to the entire request; other errors are returned.
    pub fn continue_at_step_limit(mut self, enabled: bool) -> Self {
        self.continue_at_step_limit = enabled;
        self
    }

    pub fn image(mut self, image: Image) -> Self {
        self.images.push(image);
        self
    }

    pub fn image_path(mut self, path: impl AsRef<Path>) -> Result<Self, SdkError> {
        let path = path.as_ref();
        let data = std::fs::read(path).map_err(|source| SdkError::Image {
            path: path.to_path_buf(),
            source,
        })?;
        let media_type = match path.extension().and_then(|value| value.to_str()) {
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            _ => {
                return Err(SdkError::Config(format!(
                    "unsupported image type: {}",
                    path.display()
                )))
            }
        };
        self.images.push(Image {
            media_type: media_type.into(),
            data: base64_encode(&data),
        });
        Ok(self)
    }

    pub fn deadline(mut self, duration: Duration) -> Self {
        self.deadline = Some(duration);
        self
    }

    pub fn on_event(mut self, callback: impl Fn(HarnessEvent) + Send + Sync + 'static) -> Self {
        self.on_event = Some(Arc::new(callback));
        self
    }

    /// Capacity of the observation channel a background run feeds (see
    /// [`RunHandle::events`]); default [`DEFAULT_EVENT_CAPACITY`], and a
    /// value below one is raised to one. The stream is observational and
    /// lossy under backpressure: the run never waits for a reader, so
    /// events that arrive while the channel is full are dropped and
    /// reported through [`RunEvent::Overflow`] and
    /// [`RunOutcome::dropped_events`]. The terminal outcome is
    /// authoritative regardless. Ignored by
    /// [`Session::run`](crate::Session::run), which has no channel.
    pub fn event_capacity(mut self, capacity: usize) -> Self {
        self.event_capacity = capacity.max(1);
        self
    }
}

impl From<&str> for RunRequest {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for RunRequest {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// The convenience view of a successful run; see [`RunOutcome`] for the
/// detailed one.
#[derive(Debug, Clone)]
pub struct RunResult {
    pub text: String,
    pub usage: Usage,
    pub metered_steps: u64,
    pub messages: Vec<Message>,
}

/// Everything a run reports when it ends, whichever way it ended. The
/// accounting fields are filled in on failure and cancellation as well as
/// on success, and a persistence failure is reported next to the run's
/// own result rather than in its place.
#[derive(Debug)]
pub struct RunOutcome {
    /// The final assistant text, or why the run stopped: the kernel's
    /// [`HarnessError`](crate::HarnessError) (`Cancelled`,
    /// `DeadlineExceeded`, `StepLimitExceeded`, `Model`, ...) wrapped in
    /// [`SdkError::Harness`].
    pub execution: Result<String, SdkError>,
    /// Token usage metered over the whole run, including the steps that
    /// completed before a failure.
    pub usage: Usage,
    /// Model steps the usage meter saw.
    pub metered_steps: u64,
    /// The transcript as the run left it, including a partial tool
    /// exchange when the run stopped mid-step.
    pub messages: Vec<Message>,
    /// Whether the transcript and recovery store were saved (always `Ok`
    /// for an ephemeral session). Attempted after every run, whatever
    /// `execution` holds.
    pub persistence: Result<(), SdkError>,
    /// Events the observation channel could not hold; see
    /// [`RunRequest::event_capacity`]. Always zero for a run without a
    /// [`RunHandle`].
    pub dropped_events: u64,
}

impl RunOutcome {
    /// Both the run and its persistence succeeded.
    pub fn is_success(&self) -> bool {
        self.execution.is_ok() && self.persistence.is_ok()
    }

    /// Collapse to the convenience result: the execution error if any,
    /// otherwise the persistence error if any, otherwise the
    /// [`RunResult`]. The partial accounting is lost on the error paths.
    pub fn into_result(self) -> Result<RunResult, SdkError> {
        let text = self.execution?;
        self.persistence?;
        Ok(RunResult {
            text,
            usage: self.usage,
            metered_steps: self.metered_steps,
            messages: self.messages,
        })
    }
}

/// One item of a background run's observation stream.
#[derive(Debug, Clone)]
pub enum RunEvent {
    /// A lifecycle event, in emission order.
    Harness(HarnessEvent),
    /// `dropped` events were discarded between the previous delivered
    /// event and the next one because the channel was full (see
    /// [`RunRequest::event_capacity`]). Emitted once per gap, as soon as
    /// capacity is available again; a gap still open when the run ends is
    /// reported by a final marker that always fits, so the markers in a
    /// fully drained stream sum to [`RunOutcome::dropped_events`].
    Overflow { dropped: u64 },
}

impl RunEvent {
    /// The lifecycle event, if this is one.
    pub fn harness(self) -> Option<HarnessEvent> {
        match self {
            Self::Harness(event) => Some(event),
            Self::Overflow { .. } => None,
        }
    }
}

/// A background run started with [`Session::start`](crate::Session::start).
/// Dropping the handle cancels the run.
pub struct RunHandle {
    cancellation: CancellationToken,
    events: mpsc::Receiver<RunEvent>,
    events_taken: bool,
    dropped: Arc<AtomicU64>,
    task: Option<tokio::task::JoinHandle<Result<RunOutcome, SdkError>>>,
}

impl RunHandle {
    pub(crate) fn new(
        cancellation: CancellationToken,
        events: mpsc::Receiver<RunEvent>,
        dropped: Arc<AtomicU64>,
        task: tokio::task::JoinHandle<Result<RunOutcome, SdkError>>,
    ) -> Self {
        Self {
            cancellation,
            events,
            events_taken: false,
            dropped,
            task: Some(task),
        }
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// The observation stream: lifecycle events as they happen, with an
    /// [`RunEvent::Overflow`] marker wherever a full channel lost some.
    /// The run never waits for this receiver; the terminal outcome is
    /// authoritative. After [`take_events`](Self::take_events) this is a
    /// closed receiver that yields `None`.
    pub fn events(&mut self) -> &mut mpsc::Receiver<RunEvent> {
        &mut self.events
    }

    /// Move the observation receiver out, to read it from another task
    /// or to drop it: a dropped receiver ends observation without
    /// counting anything as dropped. `None` once already taken.
    pub fn take_events(&mut self) -> Option<mpsc::Receiver<RunEvent>> {
        if self.events_taken {
            return None;
        }
        self.events_taken = true;
        let (_closed, replacement) = mpsc::channel(1);
        Some(std::mem::replace(&mut self.events, replacement))
    }

    /// Events dropped by the observation channel so far.
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Acquire)
    }

    /// Wait for the run and collapse its outcome as
    /// [`RunOutcome::into_result`] does.
    pub async fn finish(self) -> Result<RunResult, SdkError> {
        self.outcome().await?.into_result()
    }

    /// Wait for the run's detailed outcome. `Err` only when the run never
    /// executed: the task panicked or was aborted, or a continuation had
    /// nothing to continue ([`SdkError::InvalidContext`]).
    pub async fn outcome(mut self) -> Result<RunOutcome, SdkError> {
        self.task
            .take()
            .expect("run task available")
            .await
            .map_err(|error| SdkError::Config(format!("run task failed: {error}")))?
    }
}

impl Drop for RunHandle {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        output.push(TABLE[(a >> 2) as usize] as char);
        output.push(TABLE[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(c & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}
