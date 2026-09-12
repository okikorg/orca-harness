//! One cancellable MCP reconciliation at a time, independent of UI/model work.

use std::future::Future;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::msg::{UiMsg, WorkerCmd};

#[derive(Default)]
pub(super) struct McpReload {
    task: Option<JoinHandle<()>>,
    pending: bool,
}

impl McpReload {
    pub fn request(
        &mut self,
        mcp: &crate::mcp::McpServers,
        ui: &mpsc::UnboundedSender<UiMsg>,
        worker: &mpsc::UnboundedSender<WorkerCmd>,
    ) {
        let mcp = mcp.clone();
        let worker = worker.clone();
        if self.start(async move {
            let notices = mcp.reload().await;
            let _ = worker.send(WorkerCmd::McpReloaded { notices });
        }) {
            let _ = ui.send(UiMsg::McpConnecting(true));
            let _ = ui.send(UiMsg::Notice(
                "MCP connecting in background · integration tools may be unavailable until initialization completes".into(),
            ));
        }
    }

    fn start(&mut self, work: impl Future<Output = ()> + Send + 'static) -> bool {
        if self.task.is_some() {
            // Reread the latest config once this pass finishes, rather than
            // racing two reconciliations or dropping a toggle made mid-connect.
            self.pending = true;
            return false;
        }
        self.task = Some(tokio::spawn(work));
        true
    }

    /// Returns whether a config change requested another pass.
    pub fn finish(&mut self) -> bool {
        self.task.take();
        std::mem::take(&mut self.pending)
    }
}

impl Drop for McpReload {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // Dropping the connection futures drops their kill-on-drop children.
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn stalled_reload_does_not_block_and_requests_coalesce() {
        let mut reload = McpReload::default();
        let (release, blocked) = oneshot::channel();
        let (done, completed) = oneshot::channel();
        assert!(reload.start(async move {
            let _ = blocked.await;
            let _ = done.send(());
        }));
        assert!(!reload.start(async { panic!("overlapping reload") }));
        assert!(!reload.start(async { panic!("overlapping reload") }));
        release.send(()).unwrap();
        completed.await.unwrap();
        assert!(reload.finish());
        assert!(reload.start(async {}));
        assert!(!reload.finish());
    }

    #[tokio::test]
    async fn shutdown_drops_in_flight_work() {
        let mut reload = McpReload::default();
        let (started, running) = oneshot::channel();
        let (dropped, observed) = oneshot::channel::<()>();
        reload.start(async move {
            let _held_until_cancelled = dropped;
            let _ = started.send(());
            std::future::pending::<()>().await;
        });
        running.await.unwrap();
        drop(reload);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), observed)
                .await
                .expect("reload task should be cancelled")
                .is_err()
        );
    }
}
