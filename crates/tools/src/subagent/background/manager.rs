use super::super::{SubagentDepth, SubagentSpawn};
use serde_json::Value;
use std::sync::{atomic::AtomicU64, Arc};

#[cfg(test)]
mod tests;

/// No default concurrency limit. Zero means unrestricted; an explicit positive
/// limit queues excess workers in spawn order and can be changed live.
pub const DEFAULT_BACKGROUND_SUBAGENT_LIMIT: u32 = 0;

/// Terminal result of one detached subagent, delivered to the interactive
/// host after the original tool call has already returned its acknowledgement.
#[derive(Clone, Debug)]
pub struct SubagentNotification {
    pub generation: u64,
    pub spawn: SubagentSpawn,
    pub result: Result<Value, String>,
}

/// Where an admitted background job is in its lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundStatus {
    /// Admitted and waiting, in spawn order, for a running slot.
    Queued,
    /// Holds a slot and is executing model or tool steps.
    Running,
}

impl BackgroundStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
        }
    }
}

/// One admitted background job as `action=list` reports it.
#[derive(Clone, Debug)]
pub struct BackgroundJob {
    pub spawn: SubagentSpawn,
    pub status: BackgroundStatus,
}

struct SubagentManagerState {
    generation: u64,
    cancellation: orca_harness_core::CancellationToken,
    jobs: std::collections::HashMap<u64, ActiveSubagent>,
    /// Admitted spawns still waiting for a slot, oldest first.
    queue: std::collections::VecDeque<u64>,
    /// Live slot guards across all notification generations. Cancellation
    /// requests stop work, but capacity is released only when workers exit.
    running: usize,
    completion_capacity: Option<usize>,
    /// Reservations survive worker completion until the host consumes it.
    unacknowledged: std::collections::HashSet<u64>,
}

struct ActiveSubagent {
    spawn: SubagentSpawn,
    cancellation: orca_harness_core::CancellationToken,
    status: BackgroundStatus,
}

pub(super) struct SubagentManagerInner {
    state: std::sync::Mutex<SubagentManagerState>,
    /// Source of the live concurrency limit. Held here so its watch stays
    /// open for as long as any job may wait on it.
    settings: SubagentDepth,
    /// Bumped whenever a slot frees or the queue head changes; every
    /// waiter re-checks whether it is next.
    slots: tokio::sync::watch::Sender<u64>,
    pub(super) spawn_seq: Arc<AtomicU64>,
}

impl Drop for SubagentManagerInner {
    fn drop(&mut self) {
        self.state.get_mut().unwrap().cancellation.cancel();
    }
}

/// Session-owned lifetime and concurrency boundary for detached subagents.
#[derive(Clone)]
pub struct SubagentManager {
    pub(super) inner: Arc<SubagentManagerInner>,
    _lifetime: Arc<ManagerLifetime>,
}

struct ManagerLifetime(std::sync::Weak<SubagentManagerInner>);

impl Drop for ManagerLifetime {
    fn drop(&mut self) {
        if let Some(inner) = self.0.upgrade() {
            inner.cancel_all();
        }
    }
}

impl Default for SubagentManager {
    fn default() -> Self {
        Self::new(DEFAULT_BACKGROUND_SUBAGENT_LIMIT)
    }
}

/// A running slot. Dropping it frees the slot and wakes the queue, however
/// the job ends.
pub(crate) struct Slot(Arc<SubagentManagerInner>);

/// Rolls registration back if host extension setup fails before detachment.
pub(crate) struct Admission {
    manager: Arc<SubagentManagerInner>,
    generation: u64,
    spawn_id: u64,
    cancellation: orca_harness_core::CancellationToken,
    slot: Option<Slot>,
    transferred: bool,
}

impl Admission {
    pub(crate) fn into_parts(
        mut self,
    ) -> (u64, orca_harness_core::CancellationToken, Option<Slot>) {
        self.transferred = true;
        (self.generation, self.cancellation.clone(), self.slot.take())
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        if self.transferred {
            return;
        }
        let mut state = self.manager.state.lock().unwrap();
        if state.generation == self.generation {
            state.jobs.remove(&self.spawn_id);
            state.queue.retain(|id| *id != self.spawn_id);
            state.unacknowledged.remove(&self.spawn_id);
        }
        drop(state);
        self.manager.wake();
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.running = state.running.saturating_sub(1);
        drop(state);
        self.0.wake();
    }
}

impl SubagentManager {
    /// A manager with a fixed limit, for hosts without a `/subagents`
    /// settings handle.
    pub fn new(max_concurrent: u32) -> Self {
        let settings = SubagentDepth::default();
        settings.set_background_limit(max_concurrent);
        Self::from_settings(settings)
    }

    /// A manager whose limit follows `settings.background_limit()` live:
    /// raising it admits queued jobs at once.
    pub fn from_settings(settings: SubagentDepth) -> Self {
        let inner = Arc::new(SubagentManagerInner {
            state: std::sync::Mutex::new(SubagentManagerState {
                generation: 0,
                cancellation: orca_harness_core::CancellationToken::new(),
                jobs: std::collections::HashMap::new(),
                queue: std::collections::VecDeque::new(),
                running: 0,
                completion_capacity: None,
                unacknowledged: std::collections::HashSet::new(),
            }),
            settings,
            slots: tokio::sync::watch::Sender::new(0),
            spawn_seq: Arc::new(AtomicU64::new(0)),
        });
        Self {
            _lifetime: Arc::new(ManagerLifetime(Arc::downgrade(&inner))),
            inner,
        }
    }

    pub(crate) fn spawn_sequence(&self) -> Arc<AtomicU64> {
        self.inner.spawn_seq.clone()
    }

    /// Bound admitted jobs plus completed results awaiting host consumption.
    /// Configure before admitting jobs. Zero rejects every background spawn.
    pub fn with_completion_capacity(self, capacity: usize) -> Self {
        let mut state = self.inner.state.lock().unwrap();
        assert!(state.jobs.is_empty() && state.unacknowledged.is_empty());
        state.completion_capacity = Some(capacity);
        drop(state);
        self
    }

    /// Release a completion reservation after its result has been consumed.
    pub fn acknowledge(&self, generation: u64, spawn_id: u64) {
        let mut state = self.inner.state.lock().unwrap();
        if state.generation == generation {
            state.unacknowledged.remove(&spawn_id);
        }
    }

    pub fn cancel_all(&self) -> usize {
        self.inner.cancel_all()
    }
    pub fn cancel(&self, spawn_id: u64) -> bool {
        self.inner.cancel(spawn_id)
    }
    pub fn active(&self) -> Vec<BackgroundJob> {
        self.inner.active()
    }
    pub fn is_current(&self, generation: u64) -> bool {
        self.inner.is_current(generation)
    }
}

impl SubagentManagerInner {
    fn wake(&self) {
        self.slots
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }

    /// Register a job at the back of the queue and start it right away if
    /// it is already at the front with a free slot.
    pub(super) fn admit(
        self: &Arc<Self>,
        spawn: &SubagentSpawn,
    ) -> Result<Admission, &'static str> {
        let cap = self.settings.background_limit() as usize;
        let mut state = self.state.lock().unwrap();
        if let Some(capacity) = state.completion_capacity {
            if state.unacknowledged.len() >= capacity {
                return Err("background completion capacity reached; consume pending subagent results before spawning more");
            }
            state.unacknowledged.insert(spawn.id);
        }
        let cancellation = state.cancellation.child_token();
        state.jobs.insert(
            spawn.id,
            ActiveSubagent {
                spawn: spawn.clone(),
                cancellation: cancellation.clone(),
                status: BackgroundStatus::Queued,
            },
        );
        state.queue.push_back(spawn.id);
        let generation = state.generation;
        drop(state);
        let slot = self.try_start(spawn.id, cap).then(|| Slot(self.clone()));
        Ok(Admission {
            manager: self.clone(),
            generation,
            spawn_id: spawn.id,
            cancellation,
            slot,
            transferred: false,
        })
    }

    /// Take a slot if this spawn heads the queue and one is free under `cap`.
    fn try_start(&self, spawn_id: u64, cap: usize) -> bool {
        let mut state = self.state.lock().unwrap();
        if state
            .jobs
            .get(&spawn_id)
            .is_none_or(|job| job.cancellation.is_cancelled())
            || (cap != 0 && state.running >= cap)
            || state.queue.front() != Some(&spawn_id)
        {
            return false;
        }
        state.queue.pop_front();
        state.running += 1;
        if let Some(job) = state.jobs.get_mut(&spawn_id) {
            job.status = BackgroundStatus::Running;
        }
        let next_can_start = !state.queue.is_empty() && (cap == 0 || state.running < cap);
        drop(state);
        if next_can_start {
            self.wake();
        }
        true
    }

    fn dequeue(&self, spawn_id: u64) {
        let mut state = self.state.lock().unwrap();
        let was_head = state.queue.front() == Some(&spawn_id);
        state.queue.retain(|id| *id != spawn_id);
        drop(state);
        if was_head {
            self.wake();
        }
    }

    pub(super) async fn acquire(
        self: &Arc<Self>,
        spawn_id: u64,
        cancellation: &orca_harness_core::CancellationToken,
    ) -> Option<Slot> {
        let mut limit = self.settings.background_limit_watch();
        let mut slots = self.slots.subscribe();
        loop {
            // Mark both watches seen before checking, so a change landing
            // between the check and the wait still wakes this loop.
            let cap = *limit.borrow_and_update() as usize;
            let _ = slots.borrow_and_update();
            if self.try_start(spawn_id, cap) {
                return Some(Slot(self.clone()));
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    self.dequeue(spawn_id);
                    return None;
                }
                _ = slots.changed() => {}
                _ = limit.changed() => {}
            }
        }
    }

    /// Cancel every admitted job and begin a fresh notification generation.
    pub fn cancel_all(&self) -> usize {
        let mut state = self.state.lock().unwrap();
        let cancelled = state.jobs.len();
        state.cancellation.cancel();
        state.generation = state.generation.wrapping_add(1);
        state.cancellation = orca_harness_core::CancellationToken::new();
        state.jobs.clear();
        state.queue.clear();
        state.unacknowledged.clear();
        // Retain live slots: resetting this count would admit replacements
        // before cancelled workers exit, and their later drops would then
        // incorrectly release capacity held by those replacements.
        cancelled
    }

    pub fn cancel(&self, spawn_id: u64) -> bool {
        let state = self.state.lock().unwrap();
        let Some(job) = state.jobs.get(&spawn_id) else {
            return false;
        };
        job.cancellation.cancel();
        true
    }

    pub fn active(&self) -> Vec<BackgroundJob> {
        let state = self.state.lock().unwrap();
        let mut jobs = state
            .jobs
            .values()
            .map(|job| BackgroundJob {
                spawn: job.spawn.clone(),
                status: job.status,
            })
            .collect::<Vec<_>>();
        jobs.sort_by_key(|job| job.spawn.id);
        jobs
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.state.lock().unwrap().generation == generation
    }

    pub(super) fn finish(&self, generation: u64, spawn_id: u64) {
        let mut state = self.state.lock().unwrap();
        if state.generation == generation {
            state.jobs.remove(&spawn_id);
        }
    }
}
