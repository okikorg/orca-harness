//! Fixtures for the workflow suites: an echoing stage model that can be
//! held, the tool under test with its handles, and a bounded wait for the
//! run-level notification.
#![allow(dead_code)]

use orca_harness_core::{
    CancellationToken, Context, Model, ModelError, ModelResponse, ToolContext, ToolSchema,
};
use orca_harness_dag::Stage;
use orca_harness_tools::{
    SubagentManager, SubagentNotification, SubagentTool, WorkflowStore, WorkflowTool,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub fn ctx() -> ToolContext {
    ToolContext {
        call_id: "workflow-call".into(),
        tool_name: "workflow".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

/// Answers every stage with its rendered prompt (`dimensions` with a
/// two-item `string[]` source), recording the prompts it saw; waits on
/// `held` first when given.
#[derive(Clone)]
pub struct Echo {
    pub calls: Arc<Mutex<Vec<String>>>,
    pub held: Option<CancellationToken>,
}

#[async_trait::async_trait]
impl Model for Echo {
    async fn generate(
        &self,
        context: &Context,
        _: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        let task = context
            .messages()
            .iter()
            .rev()
            .find_map(|m| match m {
                orca_harness_core::Message::User { content, .. } => Some(content.clone()),
                _ => None,
            })
            .unwrap();
        self.calls.lock().unwrap().push(task.clone());
        if let Some(token) = &self.held {
            token.cancelled().await;
        }
        Ok(ModelResponse::final_text(if task == "dimensions" {
            "[\"a\",\"b\"]".into()
        } else {
            task
        }))
    }
}

/// The tool under test with the handles a case needs: the manager that admits
/// its stages, the parent notification stream, the recorded stage prompts, and
/// the session's output store.
pub type Harness = (
    WorkflowTool<Echo>,
    SubagentManager,
    tokio::sync::mpsc::UnboundedReceiver<SubagentNotification>,
    Arc<Mutex<Vec<String>>>,
    WorkflowStore,
);

/// One running slot; `capacity` bounds undelivered run-level results.
pub fn tool(held: Option<CancellationToken>, capacity: usize) -> Harness {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let manager = SubagentManager::new(1).with_completion_capacity(capacity);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let subagent = Arc::new(
        SubagentTool::with_tools(
            Echo {
                calls: calls.clone(),
                held,
            },
            Arc::new(Vec::new),
        )
        .background(manager.clone(), move |n| {
            let _ = tx.send(n);
        }),
    );
    let store = WorkflowStore::new();
    (
        WorkflowTool::new(subagent, store.clone()).unwrap(),
        manager,
        rx,
        calls,
        store,
    )
}

/// The run-level notification and how many stages actually ran before it.
pub async fn terminal(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SubagentNotification>,
) -> (SubagentNotification, usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stages = 0;
        loop {
            let n = rx.recv().await.unwrap();
            if n.spawn.run.is_none() {
                return (n, stages);
            }
            if !n.result.as_ref().is_ok_and(|v| v["cached"] == true) {
                stages += 1;
            }
        }
    })
    .await
    .expect("workflow must terminate")
}

pub fn stage(id: &str, prompt: &str, needs: &[&str]) -> Stage {
    let mut stage = Stage::new(id, prompt);
    stage.needs = needs.iter().map(|need| need.to_string()).collect();
    stage
}

pub async fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
    for _ in 0..500 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}
