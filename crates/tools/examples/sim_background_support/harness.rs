// -------------------------------------------------------------------- run

pub(super) type SharedModel = Arc<RetryModel<Arc<SimModel>>>;
type Completions = tokio::sync::mpsc::UnboundedReceiver<(Instant, SubagentNotification)>;

pub(super) struct Harness {
    pub(super) model: Arc<SimModel>,
    pub(super) shared: SharedModel,
    pub(super) workspace: Workspace,
    pub(super) fixture: Fixture,
    pub(super) stats: BackgroundStats,
    pub(super) timing: ToolTiming,
    pub(super) events: EventConsumer,
}

impl Harness {
    fn tool(
        &self,
        manager: Option<SubagentManager>,
    ) -> (Arc<SubagentTool<SharedModel>>, Option<Completions>) {
        let events_tx = self.events.tx.clone();
        let timing = self.timing.clone();
        let mut tool = SubagentTool::new(self.shared.clone(), &self.workspace)
            .stats(self.stats.clone())
            .spawn_extensions(Arc::new(move |spawn: &SubagentSpawn| {
                let id = spawn.id;
                let tx = events_tx.clone();
                let events = EventStream::from_fn(move |event| {
                    let _ = tx.send((Instant::now(), id, event));
                });
                let execution = events.execution_marker();
                vec![
                    Arc::new(events) as Arc<dyn Extension>,
                    Arc::new(MutationPreflight) as Arc<dyn Extension>,
                    Arc::new(timing.clone()) as Arc<dyn Extension>,
                    Arc::new(Truncation::new(8_000)) as Arc<dyn Extension>,
                    Arc::new(execution) as Arc<dyn Extension>,
                ]
            }));
        let mut done_rx = None;
        if let Some(manager) = manager {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            tool = tool.background(manager, move |notification| {
                let _ = tx.send((Instant::now(), notification));
            });
            done_rx = Some(rx);
        }
        (Arc::new(tool), done_rx)
    }

    fn call_context(index: usize) -> ToolContext {
        ToolContext {
            call_id: format!("parent-{index}"),
            tool_name: "subagent".into(),
            cancellation: CancellationToken::new(),
            deadline: None,
        }
    }

    fn check(&self, index: usize, result: &Result<Value, String>, metrics: &mut LevelMetrics) {
        let (area, topic, _) = task_for(index);
        let truth = self.fixture.truths[&(area, topic)];
        match result {
            Ok(value) => match verify(value["answer"].as_str().unwrap_or_default(), truth) {
                Ok(()) => metrics.completed += 1,
                Err(reason) => {
                    metrics.failures += 1;
                    metrics.wrong_answers += 1;
                    if metrics.failure_samples.len() < 3 {
                        metrics
                            .failure_samples
                            .push(format!("wrong answer: {reason}"));
                    }
                }
            },
            Err(error) => {
                metrics.failures += 1;
                if metrics.failure_samples.len() < 3 {
                    metrics.failure_samples.push(error.clone());
                }
            }
        }
    }

    fn collect(&self, metrics: &mut LevelMetrics, wall: Duration) {
        metrics.wall = wall;
        metrics.model_calls = self.model.calls.swap(0, Ordering::Relaxed);
        metrics.model_lag_us = self.model.take_lag();
        metrics.peak_model_in_flight = self.model.peak.load(Ordering::Relaxed);
        metrics.rejected = self.model.rejected.swap(0, Ordering::Relaxed);
        for (name, us) in self.timing.take() {
            if name == "shell" {
                metrics.shell_us.push(us);
            }
            metrics.tool_us.push(us);
        }
        metrics.tool_errors = self.timing.errors.swap(0, Ordering::Relaxed);
        // Let the consumer catch up before reading its backlog figures.
        tokio::task::block_in_place(|| std::thread::sleep(Duration::from_millis(50)));
        metrics.event_lag_us = self.events.take_lag();
        metrics.events = self.events.count.swap(0, Ordering::Relaxed);
        metrics.rss_mb = rss_mb();
    }

    /// The real path: `background: true` through the manager with `limit`
    /// running slots, `spawned` workers admitted at once.
    pub(super) async fn background_level(
        &self,
        limit: u32,
        spawned: usize,
        offset: usize,
    ) -> LevelMetrics {
        self.model.reset_peak();
        let manager = SubagentManager::new(limit);
        let (tool, done_rx) = self.tool(Some(manager.clone()));
        let mut done_rx = done_rx.expect("background receiver");
        let mut metrics = LevelMetrics::default();
        let mut spawn_at: HashMap<u64, (usize, Instant)> = HashMap::new();
        let started = Instant::now();
        for i in 0..spawned {
            let index = offset + i;
            let (_, _, task) = task_for(index);
            let ack = tool
                .call(
                    json!({"task": task, "background": true}),
                    &Self::call_context(index),
                )
                .await;
            match ack {
                Ok(ack) => {
                    let id = ack["spawnId"].as_u64().expect("spawnId");
                    spawn_at.insert(id, (index, Instant::now()));
                }
                Err(error) => {
                    metrics.failures += 1;
                    metrics
                        .failure_samples
                        .push(format!("admission failed: {error}"));
                }
            }
        }
        let deadline = Duration::from_secs(900);
        for received in 0..spawn_at.len() {
            let Ok(Some((at, notification))) = tokio::time::timeout(deadline, done_rx.recv()).await
            else {
                metrics.failures += spawn_at.len() - received;
                metrics
                    .failure_samples
                    .push("timed out waiting for completions".into());
                break;
            };
            let (index, spawned_at) = spawn_at[&notification.spawn.id];
            metrics
                .latency_us
                .push(at.duration_since(spawned_at).as_micros() as u64);
            self.check(index, &notification.result, &mut metrics);
        }
        let wall = started.elapsed();
        manager.cancel_all();
        self.collect(&mut metrics, wall);
        metrics
    }

    /// The same worker through foreground calls, all at once, to probe past
    /// the background manager. The runtime shape is identical: one task per
    /// worker, one inner agent each.
    pub(super) async fn raw_level(&self, spawned: usize, offset: usize) -> LevelMetrics {
        self.model.reset_peak();
        let (tool, _) = self.tool(None);
        let mut metrics = LevelMetrics::default();
        let started = Instant::now();
        let tasks = (0..spawned)
            .map(|i| {
                let index = offset + i;
                let tool = tool.clone();
                tokio::spawn(async move {
                    let (_, _, task) = task_for(index);
                    let at = Instant::now();
                    let result = tool
                        .call(json!({"task": task}), &Self::call_context(index))
                        .await
                        .map_err(|error| error.to_string());
                    (index, at.elapsed(), result)
                })
            })
            .collect::<Vec<_>>();
        for task in tasks {
            match task.await {
                Ok((index, latency, result)) => {
                    metrics.latency_us.push(latency.as_micros() as u64);
                    self.check(index, &result, &mut metrics);
                }
                Err(error) => {
                    metrics.failures += 1;
                    metrics.failure_samples.push(format!("join: {error}"));
                }
            }
        }
        let wall = started.elapsed();
        self.collect(&mut metrics, wall);
        metrics
    }
}

pub(super) fn env_list(name: &str, default: &str) -> Vec<usize> {
    std::env::var(name)
        .unwrap_or_else(|_| default.into())
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().parse().expect("comma-separated integers"))
        .collect()
}

pub(super) fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

pub(super) struct Row {
    pub(super) mode: &'static str,
    pub(super) limit: usize,
    pub(super) spawned: usize,
    pub(super) reps: usize,
    pub(super) metrics: LevelMetrics,
}

pub(super) fn print_row(row: &Row) {
    let m = &row.metrics;
    let lat = sorted(m.latency_us.clone());
    let lag = sorted(m.model_lag_us.clone());
    let tool = sorted(m.tool_us.clone());
    let shell = sorted(m.shell_us.clone());
    let ev = sorted(m.event_lag_us.clone());
    let secs = |us: u64| us as f64 / 1e6;
    let ms = |us: u64| us as f64 / 1e3;
    println!(
        "{},{},{},{},{:.2},{:.3},{:.2},{:.2},{:.2},{},{:.1},{:.1},{:.1},{},{},{},{:.1},{:.1},{:.1},{:.1},{},{},{:.1},{:.1},{},{},{:.0}",
        row.mode,
        row.limit,
        row.spawned,
        row.reps,
        m.wall.as_secs_f64(),
        (row.spawned * row.reps) as f64 / m.wall.as_secs_f64().max(1e-9),
        secs(percentile(&lat, 0.5)),
        secs(percentile(&lat, 0.95)),
        secs(percentile(&lat, 1.0)),
        m.model_calls,
        ms(percentile(&lag, 0.5)),
        ms(percentile(&lag, 0.95)),
        ms(percentile(&lag, 1.0)),
        m.peak_model_in_flight,
        m.rejected,
        m.tool_us.len(),
        ms(percentile(&tool, 0.5)),
        ms(percentile(&tool, 0.95)),
        ms(percentile(&tool, 1.0)),
        ms(percentile(&shell, 0.95)),
        m.tool_errors,
        m.events,
        ms(percentile(&ev, 0.95)),
        ms(percentile(&ev, 1.0)),
        m.failures,
        m.wrong_answers,
        m.rss_mb,
    );
}

pub(super) fn print_summary(rows: &[Row], gated: bool) {
    let Some(base) = rows.first() else {
        return;
    };
    let base_lat = percentile(&sorted(base.metrics.latency_us.clone()), 0.5).max(1) as f64;
    let base_tool = percentile(&sorted(base.metrics.tool_us.clone()), 0.95).max(1) as f64;
    eprintln!();
    eprintln!(
        "{:<11}{:>6}{:>8}{:>10}{:>10}{:>9}{:>10}{:>10}{:>9}{:>8}{:>7}{:>8}",
        "mode",
        "limit",
        "agents",
        "lat p50",
        "lat p95",
        "x base",
        "tool p95",
        "x base",
        "lag p95",
        "ev p95",
        "fail",
        "rss"
    );
    let mut knee: Option<&Row> = None;
    for row in rows {
        let m = &row.metrics;
        let lat = sorted(m.latency_us.clone());
        let tool = sorted(m.tool_us.clone());
        let lag = sorted(m.model_lag_us.clone());
        let ev = sorted(m.event_lag_us.clone());
        let lat_x = percentile(&lat, 0.5) as f64 / base_lat;
        let tool_x = percentile(&tool, 0.95) as f64 / base_tool;
        eprintln!(
            "{:<11}{:>6}{:>8}{:>9.2}s{:>9.2}s{:>8.2}x{:>8.1}ms{:>8.1}x{:>7.1}ms{:>6.1}ms{:>7}{:>6.0}M",
            row.mode,
            row.limit,
            row.spawned * row.reps,
            percentile(&lat, 0.5) as f64 / 1e6,
            percentile(&lat, 0.95) as f64 / 1e6,
            lat_x,
            percentile(&tool, 0.95) as f64 / 1e3,
            tool_x,
            percentile(&lag, 0.95) as f64 / 1e3,
            percentile(&ev, 0.95) as f64 / 1e3,
            m.failures,
            m.rss_mb,
        );
        // The knee: the first level where a worker's own latency inflates
        // by half, the runtime falls a second behind the model's clock, or
        // work stops being correct.
        let lagging = percentile(&lag, 0.95) > 1_000_000;
        if knee.is_none() && (m.failures > 0 || lat_x > 1.5 || lagging) {
            knee = Some(row);
        }
    }
    eprintln!();
    if gated {
        eprintln!("provider admission enabled: worker latency includes queue wait; no harness knee inferred");
        return;
    }
    match knee {
        Some(row) => eprintln!(
            "knee: {} at {} concurrent workers (failures={}, samples: {:?})",
            row.mode, row.limit, row.metrics.failures, row.metrics.failure_samples
        ),
        None => eprintln!(
            "no knee within the sweep: every level completed correctly with latency within \
             1.5x of one worker and the runtime keeping the model's clock"
        ),
    }
}
