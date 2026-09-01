//! Live counters for host status displays: how many process-tool
//! children, Python kernels, Bun REPLs, and in-flight subagents exist right
//! now. Cloneable and lock-free; a default instance nobody reads costs nothing. The
//! tools mutate the counters; hosts normally only read them (the
//! increment/decrement methods are public so tests and custom tools can
//! participate).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct BackgroundStats {
    processes: Arc<AtomicUsize>,
    kernels: Arc<AtomicUsize>,
    bun_repls: Arc<AtomicUsize>,
    agents: Arc<AtomicUsize>,
}

impl BackgroundStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn processes(&self) -> usize {
        self.processes.load(Ordering::Relaxed)
    }
    pub fn kernels(&self) -> usize {
        self.kernels.load(Ordering::Relaxed)
    }
    pub fn bun_repls(&self) -> usize {
        self.bun_repls.load(Ordering::Relaxed)
    }
    pub fn agents(&self) -> usize {
        self.agents.load(Ordering::Relaxed)
    }

    pub fn inc_processes(&self) {
        self.processes.fetch_add(1, Ordering::Relaxed);
    }
    pub fn dec_processes(&self) {
        self.processes.fetch_sub(1, Ordering::Relaxed);
    }
    pub fn inc_kernels(&self) {
        self.kernels.fetch_add(1, Ordering::Relaxed);
    }
    pub fn dec_kernels(&self) {
        self.kernels.fetch_sub(1, Ordering::Relaxed);
    }
    pub fn inc_bun_repls(&self) {
        self.bun_repls.fetch_add(1, Ordering::Relaxed);
    }
    pub fn dec_bun_repls(&self) {
        self.bun_repls.fetch_sub(1, Ordering::Relaxed);
    }
    pub fn inc_agents(&self) {
        self.agents.fetch_add(1, Ordering::Relaxed);
    }
    pub fn dec_agents(&self) {
        self.agents.fetch_sub(1, Ordering::Relaxed);
    }
}
