//! Child startup, output readers, and lifetime supervision.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use orca_harness_core::{CancellationToken, ToolError};
use tokio::sync::Notify;

use super::output::OutBuf;
use super::{
    settle_exit, Proc, ProcessCore, ProcessNotificationKind, ProcessSnapshot, ProcessSpawn,
};
use crate::pgroup;

impl ProcessCore<'_> {
    pub(super) async fn spawn(
        &self,
        spawn: ProcessSpawn,
        cancellation: &CancellationToken,
    ) -> Result<ProcessSnapshot, ToolError> {
        let ProcessSpawn {
            command: command_str,
            wait_for_exit,
            notify_on_exit,
            notify_on_match: notify_match,
        } = spawn;
        if command_str.trim().is_empty() {
            return Err(ToolError::msg("`command` must not be empty"));
        }
        if notify_match.as_deref() == Some("") {
            return Err(ToolError::msg("`notifyOnMatch` must not be empty"));
        }
        let notify_on_exit = !wait_for_exit && notify_on_exit;
        self.ensure_open()?;
        let config = self.config;
        let manager = self.manager;

        // Claim a slot before the child exists: the reservation is what
        // keeps parallel spawns from all passing the same count, and it is
        // released by `Slot`'s drop on every path that returns early.
        let slot = manager.reserve_slot(config.max_processes)?;

        let (spawner, request) = config
            .executor
            .spawn_parts(&command_str, config.working_dir.as_deref());
        let sandboxed = spawner.is_sandboxed();
        let (mut spawned, mut output) = spawner.spawn(request).await?;
        let pgid = spawned.pgid();
        let stdin = spawned.take_stdin();

        let id = format!("p{}", manager.seq.fetch_add(1, Ordering::SeqCst) + 1);
        let notification_cap = usize::from(
            config.notifier.is_some()
                && !wait_for_exit
                && (notify_on_exit || notify_match.is_some()),
        ) * config.max_output_bytes;
        let proc = Arc::new(Proc {
            id: id.clone(),
            command: command_str.clone(),
            pgid,
            counted: AtomicBool::new(true),
            buf: Mutex::new(OutBuf::new(
                config.buffer_cap,
                notification_cap,
                (!wait_for_exit).then_some(notify_match).flatten(),
            )),
            output_ready: Notify::new(),
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            exit: Mutex::new(None),
            kill: manager.shutdown.child_token(),
            done: CancellationToken::new(),
            notify_on_exit: AtomicBool::new(false),
            notifier: config.notifier.clone(),
        });

        // One reader for both streams: they were already merged into a
        // single unread buffer (terminal semantics), and the spawner
        // hands them over interleaved in arrival order.
        //
        // `drained` firing means the output stream closed, which is the
        // one end-of-process signal every sandbox provider has.
        let drained = CancellationToken::new();
        let reader = {
            let p = proc.clone();
            let drained = drained.clone();
            tokio::spawn(async move {
                while let Some(chunk) = output.recv().await {
                    p.append_output(&chunk.bytes);
                }
                drained.cancel();
            })
        };

        manager.stats.add_process(id.clone(), command_str);

        // The waiter owns the child: reap on exit or kill on demand, then
        // give the readers a moment to drain before signalling `done`.
        let p = proc.clone();
        let stats = manager.stats.clone();
        tokio::spawn(async move {
            let exit = tokio::select! {
                biased;
                _ = p.kill.cancelled() => {
                    // The group kill reaches grandchildren; `Spawned::kill`
                    // then ends and reaps the child itself.
                    if let Some(pgid) = p.pgid {
                        pgroup::kill_group(pgid);
                    }
                    spawned.kill().await;
                    None
                }
                code = spawned.wait() => code,
                // Only under a sandbox. Locally, a forked grandchild can
                // hold the pipes open past its parent's exit, so the
                // stream closing is not the parent ending — `wait` is the
                // authority there, and this arm would report exit early.
                _ = drained.cancelled(), if sandboxed => {
                    // The stream usually closes just before the provider
                    // reports the code, so this arm wins the race even
                    // where a code was available. Wait it out rather than
                    // discarding an exit code we could have had; only a
                    // provider that reports none pays the full delay, and
                    // only once its process has already ended.
                    tokio::time::timeout(Duration::from_millis(500), spawned.wait())
                        .await
                        .ok()
                        .flatten()
                }
            };
            *p.exit.lock().unwrap() = Some(exit);
            if p.counted.swap(false, Ordering::Relaxed) {
                stats.remove_process(&p.id);
                stats.dec_processes();
            }
            let _ = tokio::time::timeout(Duration::from_millis(200), reader).await;
            p.done.cancel();
            if p.notify_on_exit.load(Ordering::Acquire) {
                p.emit(ProcessNotificationKind::Exit {
                    exit_code: p.exit_code(),
                });
            }
            p.output_ready.notify_waiters();
        });

        slot.commit(id.clone(), proc.clone());

        if wait_for_exit {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(ToolError::msg("cancelled")),
                _ = proc.done.cancelled() => {}
            }
        } else {
            // Give fast-failing commands a chance to report immediately.
            let _ = tokio::time::timeout(config.settle, proc.done.cancelled()).await;
        }
        settle_exit(&proc).await;
        Ok(self.snapshot(&id, &proc, notify_on_exit, !wait_for_exit))
    }
}
