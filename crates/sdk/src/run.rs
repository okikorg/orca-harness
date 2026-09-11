use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::{CancellationToken, Image, Message, Usage};
use orca_harness_extensions::HarnessEvent;

use crate::SdkError;

pub type EventCallback = Arc<dyn Fn(HarnessEvent) + Send + Sync>;

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
    /// Images cannot be attached (there is no message to carry them) and
    /// are rejected with [`SdkError::Config`] when the run starts. A
    /// session whose transcript holds nothing beyond the system prompt
    /// rejects the request with [`SdkError::Config`].
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

    /// Whether this request was built with [`RunRequest::continuation`].
    pub fn is_continuation(&self) -> bool {
        self.continuation
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

#[derive(Debug, Clone)]
pub struct RunResult {
    pub text: String,
    pub usage: Usage,
    pub metered_steps: u64,
    pub messages: Vec<Message>,
}

pub struct RunHandle {
    cancellation: CancellationToken,
    events: tokio::sync::mpsc::UnboundedReceiver<HarnessEvent>,
    task: Option<tokio::task::JoinHandle<Result<RunResult, SdkError>>>,
}

impl RunHandle {
    pub(crate) fn new(
        cancellation: CancellationToken,
        events: tokio::sync::mpsc::UnboundedReceiver<HarnessEvent>,
        task: tokio::task::JoinHandle<Result<RunResult, SdkError>>,
    ) -> Self {
        Self {
            cancellation,
            events,
            task: Some(task),
        }
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub fn events(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<HarnessEvent> {
        &mut self.events
    }

    pub async fn finish(mut self) -> Result<RunResult, SdkError> {
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
