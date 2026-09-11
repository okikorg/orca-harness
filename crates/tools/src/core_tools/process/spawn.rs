//! Child startup, output readers, and lifetime supervision.

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use orca_harness_core::{CancellationToken, ToolError};
use tokio::io::AsyncReadExt;
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

        if manager.procs.lock().unwrap().len() >= config.max_processes {
            return Err(ToolError::msg(format!(
                "live process limit reached ({}); kill one first",
                config.max_processes
            )));
        }

        let mut cmd = config
            .executor
            .build(&command_str, config.working_dir.as_deref());
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::msg(format!("failed to spawn: {e}")))?;
        let pgid = child.id();

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();

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
            stdin: tokio::sync::Mutex::new(stdin),
            exit: Mutex::new(None),
            kill: manager.shutdown.child_token(),
            done: CancellationToken::new(),
            notify_on_exit: AtomicBool::new(false),
            notifier: config.notifier.clone(),
        });

        let mut readers = Vec::new();
        for pipe in [stdout.map(either::Left), stderr.map(either::Right)] {
            let Some(pipe) = pipe else { continue };
            let p = proc.clone();
            readers.push(tokio::spawn(async move {
                let mut chunk = [0u8; 8192];
                match pipe {
                    either::Either::Left(mut out) => loop {
                        match out.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                p.append_output(&chunk[..n]);
                            }
                        }
                    },
                    either::Either::Right(mut err) => loop {
                        match err.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                p.append_output(&chunk[..n]);
                            }
                        }
                    },
                }
            }));
        }

        manager.stats.add_process(id.clone(), command_str);

        // The waiter owns the child: reap on exit or kill on demand, then
        // give the readers a moment to drain before signalling `done`.
        let p = proc.clone();
        let stats = manager.stats.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                biased;
                _ = p.kill.cancelled() => {
                    if let Some(pgid) = p.pgid {
                        pgroup::kill_group(pgid);
                    }
                    let _ = child.start_kill();
                    child.wait().await
                }
                status = child.wait() => status,
            };
            *p.exit.lock().unwrap() = Some(status.ok().and_then(|s| s.code()));
            if p.counted.swap(false, Ordering::Relaxed) {
                stats.remove_process(&p.id);
                stats.dec_processes();
            }
            let _ = tokio::time::timeout(Duration::from_millis(200), async {
                for r in readers {
                    let _ = r.await;
                }
            })
            .await;
            p.done.cancel();
            if p.notify_on_exit.load(Ordering::Acquire) {
                p.emit(ProcessNotificationKind::Exit {
                    exit_code: p.exit_code(),
                });
            }
            p.output_ready.notify_waiters();
        });

        manager
            .procs
            .lock()
            .unwrap()
            .insert(id.clone(), proc.clone());

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

/// Tiny stand-in for the `either` crate so both pipe types share one
/// reader loop without a dependency.
mod either {
    pub enum Either<L, R> {
        Left(L),
        Right(R),
    }
    pub use Either::{Left, Right};
}
