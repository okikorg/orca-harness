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
}

impl RunRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            images: Vec::new(),
            deadline: None,
            on_event: None,
        }
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
