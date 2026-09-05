use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

/// Default governance for workers: enough room for a focused tool task, but
/// deliberately below the orchestrator's budget.
pub const DEFAULT_SUBAGENT_MAX_STEPS: u32 = 24;
pub const DEFAULT_SUBAGENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// A worker needs at least one nesting level and one model step.
pub const MIN_SUBAGENT_DEPTH: u32 = 1;
pub const MIN_SUBAGENT_MAX_STEPS: u32 = 1;
pub const AUTO_SUBAGENT_ROUTE: &str = "auto";
pub const PREFERENCE_SUBAGENT_ROUTE: &str = "preference";

const MODEL_TIERS: [&str; 4] = ["local", "flash", "mid", "frontier"];

#[derive(Debug, Clone, Copy)]
struct LiveRetry {
    attempts: u32,
    backoff_ms: u32,
    configured: bool,
}

#[derive(Debug, Default)]
struct ModelRouting {
    route: Option<String>,
    preferred: std::collections::HashMap<String, String>,
    available: Vec<String>,
}

#[derive(Debug)]
struct SubagentSettingsInner {
    depth: AtomicU32,
    max_steps: AtomicU32,
    timeout_secs: AtomicU32,
    output_chars: AtomicU32,
    /// A watch rather than an atomic: queued background workers wait on
    /// it, so raising the limit mid-session admits them immediately.
    background_limit: tokio::sync::watch::Sender<u32>,
    retry: StdMutex<LiveRetry>,
    parallel_tools: StdMutex<Option<u32>>,
    model_concurrency: tokio::sync::watch::Sender<usize>,
    model_attempts: AtomicU32,
    model_backoff_ms: StdMutex<Option<u32>>,
    model_max_backoff_ms: AtomicU32,
    routing: StdMutex<ModelRouting>,
}

/// Shared, session-live governance for spawned agents. The historical name is
/// retained for API compatibility; `/subagents` now adjusts every field.
#[derive(Clone, Debug)]
pub struct SubagentDepth(Arc<SubagentSettingsInner>);

impl Default for SubagentDepth {
    fn default() -> Self {
        Self::new(MIN_SUBAGENT_DEPTH)
    }
}

impl SubagentDepth {
    pub fn new(max_depth: u32) -> Self {
        Self(Arc::new(SubagentSettingsInner {
            depth: AtomicU32::new(clamp_depth(max_depth)),
            max_steps: AtomicU32::new(DEFAULT_SUBAGENT_MAX_STEPS),
            timeout_secs: AtomicU32::new(DEFAULT_SUBAGENT_TIMEOUT.as_secs() as u32),
            output_chars: AtomicU32::new(8_000),
            background_limit: tokio::sync::watch::Sender::new(
                super::DEFAULT_BACKGROUND_SUBAGENT_LIMIT,
            ),
            parallel_tools: StdMutex::new(None),
            model_concurrency: tokio::sync::watch::Sender::new(0),
            model_attempts: AtomicU32::new(0),
            model_backoff_ms: StdMutex::new(None),
            model_max_backoff_ms: AtomicU32::new(0),
            retry: StdMutex::new(LiveRetry {
                attempts: 1,
                backoff_ms: 250,
                configured: false,
            }),
            routing: StdMutex::new(ModelRouting::default()),
        }))
    }

    pub fn get(&self) -> u32 {
        self.0.depth.load(Ordering::Relaxed)
    }

    pub fn set(&self, max_depth: u32) -> u32 {
        let clamped = clamp_depth(max_depth);
        self.0.depth.store(clamped, Ordering::Relaxed);
        clamped
    }

    pub fn max_steps(&self) -> u32 {
        self.0.max_steps.load(Ordering::Relaxed)
    }

    pub fn set_max_steps(&self, value: u32) -> u32 {
        set_min(&self.0.max_steps, value, MIN_SUBAGENT_MAX_STEPS)
    }

    pub(super) fn set_max_steps_from_limits(&self, value: u32) {
        self.0.max_steps.store(value, Ordering::Relaxed);
    }

    pub fn timeout_secs(&self) -> u32 {
        self.0.timeout_secs.load(Ordering::Relaxed)
    }

    pub fn set_timeout_secs(&self, value: u32) -> u32 {
        set_min(&self.0.timeout_secs, value, 0)
    }

    pub fn output_chars(&self) -> u32 {
        self.0.output_chars.load(Ordering::Relaxed)
    }

    pub fn set_output_chars(&self, value: u32) -> u32 {
        set_min(&self.0.output_chars, value, 0)
    }

    /// How many detached workers may run at once; the rest queue in
    /// spawn order. Zero means unrestricted.
    pub fn background_limit(&self) -> u32 {
        *self.0.background_limit.borrow()
    }

    pub fn set_background_limit(&self, value: u32) -> u32 {
        self.0.background_limit.send_replace(value);
        value
    }

    pub(super) fn background_limit_watch(&self) -> tokio::sync::watch::Receiver<u32> {
        self.0.background_limit.subscribe()
    }

    /// None inherits the host limit; zero explicitly allows unrestricted parallel tools.
    pub fn parallel_tools(&self) -> Option<u32> {
        *self.0.parallel_tools.lock().unwrap()
    }

    pub fn set_parallel_tools(&self, value: u32) -> u32 {
        *self.0.parallel_tools.lock().unwrap() = Some(value);
        value
    }

    /// Provider controls are shared by parent and worker model wrappers.
    /// Zero concurrency/attempts/maximum delay means no configured ceiling.
    pub fn model_concurrency(&self) -> u32 {
        *self.0.model_concurrency.borrow() as u32
    }

    pub fn set_model_concurrency(&self, value: u32) -> u32 {
        self.0.model_concurrency.send_replace(value as usize);
        value
    }

    pub fn model_concurrency_watch(&self) -> tokio::sync::watch::Receiver<usize> {
        self.0.model_concurrency.subscribe()
    }

    pub fn model_attempts(&self) -> u32 {
        self.0.model_attempts.load(Ordering::Relaxed)
    }

    pub fn set_model_attempts(&self, value: u32) -> u32 {
        set_min(&self.0.model_attempts, value, 0)
    }

    /// None inherits the model wrapper's default backoff.
    pub fn model_backoff_ms(&self) -> Option<u32> {
        *self.0.model_backoff_ms.lock().unwrap()
    }

    pub fn set_model_backoff_ms(&self, value: u32) -> u32 {
        *self.0.model_backoff_ms.lock().unwrap() = Some(value);
        value
    }

    pub fn model_max_backoff_ms(&self) -> u32 {
        self.0.model_max_backoff_ms.load(Ordering::Relaxed)
    }

    pub fn set_model_max_backoff_ms(&self, value: u32) -> u32 {
        set_min(&self.0.model_max_backoff_ms, value, 0)
    }

    pub fn tool_attempts(&self) -> u32 {
        self.0.retry.lock().unwrap().attempts
    }

    pub fn set_tool_attempts(&self, value: u32) -> u32 {
        let value = value.max(1);
        let mut retry = self.0.retry.lock().unwrap();
        retry.attempts = value;
        retry.configured = true;
        value
    }

    pub fn retry_backoff_ms(&self) -> u32 {
        self.0.retry.lock().unwrap().backoff_ms
    }

    pub fn set_retry_backoff_ms(&self, value: u32) -> u32 {
        let mut retry = self.0.retry.lock().unwrap();
        retry.backoff_ms = value;
        retry.configured = true;
        value
    }

    pub(super) fn configure_retry(&self, attempts: u32, backoff_ms: u32) {
        let mut retry = self.0.retry.lock().unwrap();
        retry.attempts = attempts;
        retry.backoff_ms = backoff_ms;
        retry.configured = true;
    }

    /// Install host retry defaults once without overwriting later live choices.
    pub fn ensure_retry_defaults(&self, attempts: u32, backoff_ms: u32) {
        let mut retry = self.0.retry.lock().unwrap();
        if !retry.configured {
            retry.attempts = attempts.max(1);
            retry.backoff_ms = backoff_ms;
            retry.configured = true;
        }
    }

    pub fn model_route(&self) -> Option<String> {
        self.0.routing.lock().unwrap().route.clone()
    }

    pub fn set_model_route(&self, route: Option<String>) -> bool {
        let mut routing = self.0.routing.lock().unwrap();
        if route
            .as_ref()
            .is_some_and(|route| !route_available(&routing.available, route))
        {
            return false;
        }
        routing.route = route;
        true
    }

    pub(super) fn effective_model_route(&self) -> super::ModelRoute {
        let routing = self.0.routing.lock().unwrap();
        match routing.route.as_deref() {
            None => super::ModelRoute::Inherit,
            Some(AUTO_SUBAGENT_ROUTE) => super::ModelRoute::Auto,
            Some(PREFERENCE_SUBAGENT_ROUTE) => super::ModelRoute::Preference(
                MODEL_TIERS
                    .iter()
                    .filter_map(|tier| routing.preferred.get(*tier).cloned())
                    .collect(),
            ),
            Some(tier) => routing
                .preferred
                .get(tier)
                .cloned()
                .map(super::ModelRoute::Fixed)
                .unwrap_or(super::ModelRoute::Inherit),
        }
    }

    pub fn preferred_model(&self, tier: &str) -> Option<String> {
        self.0.routing.lock().unwrap().preferred.get(tier).cloned()
    }

    pub fn set_preferred_model(&self, tier: &str, model: String) -> bool {
        let mut routing = self.0.routing.lock().unwrap();
        if !models_for_tier(&routing.available, tier).contains(&model) {
            return false;
        }
        routing.preferred.insert(tier.to_string(), model);
        true
    }

    pub fn models_for_tier(&self, tier: &str) -> Vec<String> {
        models_for_tier(&self.0.routing.lock().unwrap().available, tier)
    }

    pub fn default_model(&self) -> Option<String> {
        let routing = self.0.routing.lock().unwrap();
        routing
            .route
            .as_ref()
            .and_then(|tier| routing.preferred.get(tier))
            .cloned()
    }

    pub fn set_default_model(&self, model: Option<String>) -> bool {
        let mut routing = self.0.routing.lock().unwrap();
        let Some(model) = model else {
            routing.route = None;
            return true;
        };
        let Some((tier, _)) = model.split_once('/') else {
            return false;
        };
        let tier = tier.to_string();
        if !models_for_tier(&routing.available, &tier).contains(&model) {
            return false;
        }
        routing.preferred.insert(tier.clone(), model);
        routing.route = Some(tier);
        true
    }

    pub fn available_models(&self) -> Vec<String> {
        self.0.routing.lock().unwrap().available.clone()
    }

    pub(super) fn set_available_models(&self, models: Vec<String>) {
        let mut routing = self.0.routing.lock().unwrap();
        routing.available = models;
        for tier in MODEL_TIERS {
            let choices = models_for_tier(&routing.available, tier);
            match routing.preferred.get(tier) {
                Some(current) if choices.contains(current) => {}
                _ => match choices.first() {
                    Some(first) => {
                        routing.preferred.insert(tier.into(), first.clone());
                    }
                    None => {
                        routing.preferred.remove(tier);
                    }
                },
            }
        }
        if routing
            .route
            .as_ref()
            .is_some_and(|route| !route_available(&routing.available, route))
        {
            routing.route = None;
        }
    }
}

fn route_available(models: &[String], route: &str) -> bool {
    if matches!(route, AUTO_SUBAGENT_ROUTE | PREFERENCE_SUBAGENT_ROUTE) {
        return true;
    }
    let prefix = format!("{route}/");
    models.iter().any(|id| id.starts_with(&prefix))
}

fn models_for_tier(models: &[String], tier: &str) -> Vec<String> {
    let prefix = format!("{tier}/");
    models
        .iter()
        .filter(|id| id.starts_with(&prefix))
        .cloned()
        .collect()
}

fn set_min(target: &AtomicU32, value: u32, min: u32) -> u32 {
    let value = value.max(min);
    target.store(value, Ordering::Relaxed);
    value
}

fn clamp_depth(depth: u32) -> u32 {
    depth.max(MIN_SUBAGENT_DEPTH)
}
