//! Stateful local tools belong to a reset generation, not an MCP catalog snapshot.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use orca_harness_tools::{BackgroundStats, BunReplTool, ProcessTool, PyKernelTool, Workspace};
use tokio::sync::mpsc;

use crate::msg::WorkerCmd;

pub(super) struct LocalTools {
    pub(super) process: Arc<ProcessTool>,
    pub(super) python: Arc<PyKernelTool>,
    pub(super) bun: Arc<BunReplTool>,
}

/// Only the current generation is retained. Replacing it releases all cached
/// references to the old tools; dropping the old agent can then stop their work.
#[derive(Default)]
pub(super) struct LocalToolsCache(Option<LocalTools>);

impl LocalToolsCache {
    pub(super) fn for_build(
        &mut self,
        preserve: bool,
        ws: &Workspace,
        stats: &BackgroundStats,
        current: &Arc<AtomicU64>,
        worker: &mpsc::UnboundedSender<WorkerCmd>,
    ) -> &LocalTools {
        if !preserve || self.0.is_none() {
            let generation = current.fetch_add(1, Ordering::AcqRel) + 1;
            let root = ws.root().to_string_lossy().into_owned();
            let process = ProcessTool::local()
                .working_dir(root.clone())
                .stats(stats.clone())
                .on_notification({
                    let worker = worker.clone();
                    let current = current.clone();
                    let sequence = AtomicU64::new(0);
                    move |notification| {
                        if current.load(Ordering::Acquire) == generation {
                            let _ = worker.send(WorkerCmd::BackgroundProcess {
                                generation,
                                sequence: sequence.fetch_add(1, Ordering::Relaxed) + 1,
                                notification,
                            });
                        }
                    }
                });
            self.0 = Some(LocalTools {
                process: Arc::new(process),
                python: Arc::new(
                    PyKernelTool::new()
                        .working_dir(root.clone())
                        .stats(stats.clone()),
                ),
                bun: Arc::new(BunReplTool::new().working_dir(root).stats(stats.clone())),
            });
        }
        self.0.as_ref().expect("local tools initialized for build")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_rebuild_preserves_identity_and_reset_releases_previous_generation() {
        let mut cache = LocalToolsCache::default();
        let ws = Workspace::new(".");
        let stats = BackgroundStats::new();
        let current = Arc::new(AtomicU64::new(0));
        let (worker, _commands) = mpsc::unbounded_channel();
        let first = cache.for_build(false, &ws, &stats, &current, &worker);
        let process = first.process.clone();
        let python = first.python.clone();
        let bun = first.bun.clone();
        assert_eq!(current.load(Ordering::Acquire), 1);

        for _ in 0..2 {
            let preserved = cache.for_build(true, &ws, &stats, &current, &worker);
            assert!(Arc::ptr_eq(&process, &preserved.process));
            assert!(Arc::ptr_eq(&python, &preserved.python));
            assert!(Arc::ptr_eq(&bun, &preserved.bun));
            assert_eq!(current.load(Ordering::Acquire), 1);
        }

        let reset = cache.for_build(false, &ws, &stats, &current, &worker);
        assert!(!Arc::ptr_eq(&process, &reset.process));
        assert!(!Arc::ptr_eq(&python, &reset.python));
        assert!(!Arc::ptr_eq(&bun, &reset.bun));
        assert_eq!(current.load(Ordering::Acquire), 2);
        let old_process = Arc::downgrade(&process);
        let old_python = Arc::downgrade(&python);
        let old_bun = Arc::downgrade(&bun);
        drop((process, python, bun));
        assert!(old_process.upgrade().is_none());
        assert!(old_python.upgrade().is_none());
        assert!(old_bun.upgrade().is_none());
    }

    #[test]
    fn preserve_without_previous_build_initializes_once() {
        let mut cache = LocalToolsCache::default();
        let current = Arc::new(AtomicU64::new(0));
        let (worker, _commands) = mpsc::unbounded_channel();
        cache.for_build(
            true,
            &Workspace::new("."),
            &BackgroundStats::new(),
            &current,
            &worker,
        );
        assert_eq!(current.load(Ordering::Acquire), 1);
    }
}
