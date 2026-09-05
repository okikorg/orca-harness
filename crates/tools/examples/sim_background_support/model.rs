// ------------------------------------------------------------------ model

/// Latency and streaming shaped like a hosted model: a time to first token
/// in the hundreds of milliseconds, then output at a fixed token rate.
pub(super) struct SimModel {
    scale: f64,
    provider_cap: Option<usize>,
    in_flight: AtomicUsize,
    peak: AtomicUsize,
    rejected: AtomicUsize,
    calls: AtomicUsize,
    /// Actual minus intended duration per call: scheduler and runtime lag.
    lag_us: Mutex<Vec<u64>>,
}

struct InFlight<'a>(&'a AtomicUsize);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(super) const TOKENS_PER_SECOND: f64 = 60.0;
const TOKENS_PER_CHUNK: usize = 4;
const CHARS_PER_TOKEN: usize = 4;

impl SimModel {
    pub(super) fn new(scale: f64, provider_cap: Option<usize>) -> Self {
        Self {
            scale,
            provider_cap,
            in_flight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            rejected: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            lag_us: Mutex::new(Vec::new()),
        }
    }

    fn scaled(&self, ms: f64) -> Duration {
        Duration::from_secs_f64(ms * self.scale / 1000.0)
    }

    fn take_lag(&self) -> Vec<u64> {
        std::mem::take(&mut *self.lag_us.lock().unwrap())
    }

    fn reset_peak(&self) {
        self.peak.store(0, Ordering::Relaxed);
    }

    /// Stream `text` in token-sized chunks at the model's output rate.
    /// Returns the intended time spent so lag can be measured.
    async fn stream(
        &self,
        sink: &dyn DeltaSink,
        text: &str,
        make: fn(String) -> ModelDelta,
    ) -> Duration {
        let chunk_ms = TOKENS_PER_CHUNK as f64 * 1000.0 / TOKENS_PER_SECOND;
        let chunk_chars = TOKENS_PER_CHUNK * CHARS_PER_TOKEN;
        let chars = text.chars().collect::<Vec<_>>();
        let mut intended = Duration::ZERO;
        for chunk in chars.chunks(chunk_chars) {
            let pause = self.scaled(chunk_ms);
            tokio::time::sleep(pause).await;
            intended += pause;
            sink.emit(make(chunk.iter().collect())).await;
        }
        intended
    }

    /// What the worker does next, from what it has seen so far.
    fn plan(context: &Context, tools: &[ToolSchema]) -> Plan {
        let task = context
            .messages()
            .iter()
            .find_map(|message| match message {
                Message::User { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .unwrap_or_default();
        let area = task
            .split("docs/")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or("area-00")
            .to_string();
        let topic = task
            .split('"')
            .nth(1)
            .unwrap_or("authentication")
            .to_string();
        let step = context
            .messages()
            .iter()
            .filter(|message| matches!(message, Message::Assistant { .. }))
            .count();
        let results = context
            .messages()
            .iter()
            .filter_map(|message| match message {
                Message::Tool { results } => Some(results),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        let has = |name: &str| tools.iter().any(|schema| schema.name == name);
        let listed = results
            .iter()
            .filter(|result| result.tool_name == "list_dir")
            .filter_map(|result| result.output["entries"].as_array())
            .flatten()
            .filter(|entry| entry["isDir"] != true)
            .filter_map(|entry| entry["name"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        let dir = format!("docs/{area}");
        let call = |id: &str, name: &str, arguments: Value| ToolCall {
            id: format!("{name}-{step}-{id}"),
            name: name.into(),
            arguments,
        };
        match step {
            0 if has("list_dir") => Plan::Calls {
                reasoning: format!(
                    "I should look at what is in {dir} before searching for {topic}."
                ),
                calls: vec![call("list", "list_dir", json!({"path": dir}))],
            },
            1 if has("grep") => {
                let mut calls = vec![call("grep", "grep", json!({"query": topic, "path": dir}))];
                for (index, name) in listed.iter().take(3).enumerate() {
                    calls.push(call(
                        &format!("read{index}"),
                        "read_file",
                        json!({"path": format!("{dir}/{name}")}),
                    ));
                }
                Plan::Calls {
                    reasoning: format!(
                        "Search the whole area for {topic:?} and read the first few notes to \
                         understand how the term is used."
                    ),
                    calls,
                }
            }
            2 if has("shell") => {
                let mut calls = Vec::new();
                if let Some(first) = listed.first() {
                    calls.push(call(
                        "sh",
                        "shell",
                        json!({"command": format!("grep -c {topic:?} {dir}/{first}")}),
                    ));
                }
                for (index, name) in listed.iter().skip(3).take(2).enumerate() {
                    calls.push(call(
                        &format!("read{index}"),
                        "read_file",
                        json!({"path": format!("{dir}/{name}")}),
                    ));
                }
                if calls.is_empty() {
                    Plan::Answer(Self::answer(&results, &dir, &topic))
                } else {
                    Plan::Calls {
                        reasoning:
                            "Cross-check one file with a shell grep and read two more notes.".into(),
                        calls,
                    }
                }
            }
            _ => Plan::Answer(Self::answer(&results, &dir, &topic)),
        }
    }

    fn answer(results: &[&orca_harness_core::ToolResult], dir: &str, topic: &str) -> String {
        let matches = results
            .iter()
            .filter(|result| result.tool_name == "grep" && !result.is_error)
            .filter_map(|result| result.output["matches"].as_array())
            .flatten()
            .collect::<Vec<_>>();
        let mut files = matches
            .iter()
            .filter_map(|m| m["path"].as_str())
            .collect::<Vec<_>>();
        files.sort_unstable();
        files.dedup();
        format!(
            "In {dir}, {topic:?} appears in {} files, {} matching lines. The shell grep on the \
             first note agreed with the search results, and the notes use the term in the \
             sense of a documented service property.",
            files.len(),
            matches.len()
        )
    }
}

enum Plan {
    Calls {
        reasoning: String,
        calls: Vec<ToolCall>,
    },
    Answer(String),
}

#[async_trait]
impl Model for SimModel {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        struct Quiet;
        #[async_trait]
        impl DeltaSink for Quiet {
            async fn emit(&self, _delta: ModelDelta) {}
        }
        self.generate_streaming(context, tools, &Quiet).await
    }

    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        let active = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        let _in_flight = InFlight(&self.in_flight);
        self.peak.fetch_max(active, Ordering::Relaxed);
        if self.provider_cap.is_some_and(|cap| active > cap) {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            // Providers answer fast when refusing; the harness's retry
            // policy decides what happens next.
            tokio::time::sleep(self.scaled(80.0)).await;
            return Err(orca_harness_model_providers::http_error::request_error(
                429,
                None,
                "concurrent stream limit",
            ));
        }
        self.calls.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let input_chars: usize = context
            .messages()
            .iter()
            .map(|message| serde_json::to_string(message).map_or(0, |s| s.len()))
            .sum();
        let mut rng = Rng::seeded(&["ttft", &input_chars.to_string()]);
        let first_token = self.scaled(450.0 + rng.range(0, 900) as f64);
        tokio::time::sleep(first_token).await;
        let mut intended = first_token;
        let mut output_chars = 0;

        let plan = Self::plan(context, tools);
        let response = match plan {
            Plan::Calls { reasoning, calls } => {
                intended += self
                    .stream(sink, &reasoning, |text| ModelDelta::Reasoning { text })
                    .await;
                output_chars += reasoning.len();
                for call in &calls {
                    let arguments = call.arguments.to_string();
                    intended += self
                        .stream(sink, &arguments, |text| ModelDelta::ToolInput { text })
                        .await;
                    output_chars += arguments.len();
                }
                let usage = Usage {
                    input_tokens: (input_chars / CHARS_PER_TOKEN) as u64,
                    output_tokens: (output_chars / CHARS_PER_TOKEN) as u64,
                    ..Default::default()
                };
                ModelResponse::ToolCalls {
                    content: None,
                    calls,
                    usage: Some(usage),
                }
            }
            Plan::Answer(text) => {
                intended += self
                    .stream(sink, &text, |text| ModelDelta::Text { text })
                    .await;
                output_chars += text.len();
                ModelResponse::Final {
                    usage: Some(Usage {
                        input_tokens: (input_chars / CHARS_PER_TOKEN) as u64,
                        output_tokens: (output_chars / CHARS_PER_TOKEN) as u64,
                        ..Default::default()
                    }),
                    text,
                }
            }
        };
        let lag = started.elapsed().saturating_sub(intended);
        self.lag_us.lock().unwrap().push(lag.as_micros() as u64);
        Ok(response)
    }
}
