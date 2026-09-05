// ---------------------------------------------------------------- metrics

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn sorted(mut samples: Vec<u64>) -> Vec<u64> {
    samples.sort_unstable();
    samples
}

fn rss_mb() -> f64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map_or(0.0, |kb| kb / 1024.0)
}

#[derive(Default)]
pub(super) struct LevelMetrics {
    wall: Duration,
    completed: usize,
    latency_us: Vec<u64>,
    model_calls: usize,
    model_lag_us: Vec<u64>,
    peak_model_in_flight: usize,
    rejected: usize,
    tool_us: Vec<u64>,
    shell_us: Vec<u64>,
    tool_errors: usize,
    pub(super) events: usize,
    event_lag_us: Vec<u64>,
    failures: usize,
    wrong_answers: usize,
    failure_samples: Vec<String>,
    rss_mb: f64,
}

impl LevelMetrics {
    pub(super) fn absorb(&mut self, other: LevelMetrics) {
        self.wall += other.wall;
        self.completed += other.completed;
        self.latency_us.extend(other.latency_us);
        self.model_calls += other.model_calls;
        self.model_lag_us.extend(other.model_lag_us);
        self.peak_model_in_flight = self.peak_model_in_flight.max(other.peak_model_in_flight);
        self.rejected += other.rejected;
        self.tool_us.extend(other.tool_us);
        self.shell_us.extend(other.shell_us);
        self.tool_errors += other.tool_errors;
        self.events += other.events;
        self.event_lag_us.extend(other.event_lag_us);
        self.failures += other.failures;
        self.wrong_answers += other.wrong_answers;
        self.failure_samples.extend(other.failure_samples);
        self.failure_samples.truncate(3);
        self.rss_mb = self.rss_mb.max(other.rss_mb);
    }
}
