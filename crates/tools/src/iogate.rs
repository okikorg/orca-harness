//! Shared filesystem I/O gate.
//!
//! The filesystem's useful concurrency is a property of the machine, not
//! of the batch: past a small width, concurrent small-file operations
//! (writes into one directory above all) contend in the kernel and get
//! slower, while the dispatcher happily admits hundreds of calls at
//! once. File tools therefore funnel their disk work through one
//! process-wide gate: every call is admitted and runs concurrently —
//! nothing is capped, queued back, or refused — but at most `width` of
//! them touch the disk at any instant. Measured on APFS with 100-call
//! batches: edits collapse ~10× past width ~16 while width 8 is the
//! throughput knee for reads, edits, and deletes alike, so a saturated
//! gate is what makes a 100-wide batch perform like the knee.
//!
//! Scan tools (`grep`, `glob`) stay outside the gate: their calls are
//! long traversals, not small operations, and serializing them under
//! the same permits would starve the quick CRUD calls they run beside.
//!
//! Override the width with `ORCA_FS_WIDTH` (0 disables the gate).

use std::sync::OnceLock;

use tokio::sync::{Semaphore, SemaphorePermit};

const DEFAULT_WIDTH: usize = 8;

fn gate() -> &'static Option<Semaphore> {
    static GATE: OnceLock<Option<Semaphore>> = OnceLock::new();
    GATE.get_or_init(|| {
        let width = std::env::var("ORCA_FS_WIDTH")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_WIDTH);
        (width > 0).then(|| Semaphore::new(width))
    })
}

/// Hold while performing filesystem operations; drop to release. `None`
/// when the gate is disabled via `ORCA_FS_WIDTH=0`.
pub(crate) async fn fs_permit() -> Option<SemaphorePermit<'static>> {
    match gate() {
        Some(semaphore) => Some(semaphore.acquire().await.expect("fs gate is never closed")),
        None => None,
    }
}
