// ------------------------------------------------------------- extensions

/// Per-tool wall time and error counts, from inside the inner agents.
#[derive(Clone, Default)]
pub(super) struct ToolTiming {
    samples: Arc<Mutex<Vec<(String, u64)>>>,
    errors: Arc<AtomicUsize>,
}

impl ToolTiming {
    fn take(&self) -> Vec<(String, u64)> {
        std::mem::take(&mut *self.samples.lock().unwrap())
    }
}

#[async_trait]
impl Extension for ToolTiming {
    fn name(&self) -> &str {
        "sim-tool-timing"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().around_tool()
    }

    async fn around_tool<'a>(
        &self,
        call: &ToolCall,
        input: Value,
        _ctx: &ToolContext,
        next: Next<'a>,
    ) -> Result<Value, ToolError> {
        let started = Instant::now();
        let result = next.run(input).await;
        if result.is_err() {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        self.samples
            .lock()
            .unwrap()
            .push((call.name.clone(), started.elapsed().as_micros() as u64));
        result
    }
}

/// Stands in for the TUI: drains every spawn's events off one channel and
/// keeps a bounded per-agent transcript, recording how far behind it runs.
pub(super) struct EventConsumer {
    tx: tokio::sync::mpsc::UnboundedSender<(Instant, u64, HarnessEvent)>,
    lag_us: Arc<Mutex<Vec<u64>>>,
    count: Arc<AtomicUsize>,
}

impl EventConsumer {
    pub(super) fn start() -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(Instant, u64, HarnessEvent)>();
        let lag_us = Arc::new(Mutex::new(Vec::new()));
        let count = Arc::new(AtomicUsize::new(0));
        let consumer_lag = lag_us.clone();
        let consumer_count = count.clone();
        tokio::spawn(async move {
            let mut transcripts: HashMap<u64, String> = HashMap::new();
            while let Some((sent, id, event)) = rx.recv().await {
                consumer_lag
                    .lock()
                    .unwrap()
                    .push(sent.elapsed().as_micros() as u64);
                consumer_count.fetch_add(1, Ordering::Relaxed);
                let text = match &event {
                    HarnessEvent::AssistantDelta { text }
                    | HarnessEvent::ReasoningDelta { text }
                    | HarnessEvent::ToolInputDelta { text } => text.as_str(),
                    HarnessEvent::Result { .. } => {
                        transcripts.remove(&id);
                        continue;
                    }
                    _ => "",
                };
                let transcript = transcripts.entry(id).or_default();
                transcript.push_str(text);
                if transcript.len() > 64 * 1024 {
                    transcript.drain(..32 * 1024);
                }
            }
        });
        Self { tx, lag_us, count }
    }

    fn take_lag(&self) -> Vec<u64> {
        std::mem::take(&mut *self.lag_us.lock().unwrap())
    }
}
