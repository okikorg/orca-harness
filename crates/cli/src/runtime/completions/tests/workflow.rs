use super::*;

#[test]
fn workflow_stages_settle_ui_without_parent_delivery_or_wake() {
    let inbox = CompletionInbox::new(SubagentManager::default());
    let (ui, mut events) = mpsc::unbounded_channel();
    let (worker, mut commands) = mpsc::unbounded_channel();
    let mut stage = notification(0, 5, "private intermediate result");
    stage.spawn.run = Some(4);
    stage.spawn.parent_id = Some(4);
    inbox.publish(stage, &ui, &worker);
    assert!(matches!(
        events.try_recv().unwrap(),
        UiMsg::SubagentCompleted { id: 5, .. }
    ));
    assert!(!inbox.has_ready());
    assert!(commands.try_recv().is_err());
    inbox.publish(notification(0, 4, "workflow final outcome"), &ui, &worker);
    assert!(matches!(
        commands.try_recv().unwrap(),
        WorkerCmd::BackgroundSubagentsReady
    ));
    assert_eq!(inbox.drain().len(), 1);
}

#[tokio::test]
async fn workflow_chain_uses_real_inbox_with_one_terminal_wake() {
    use orca_harness_core::{testing::ScriptedModel, CancellationToken, Tool, ToolContext};
    use orca_harness_tools::{SubagentTool, WorkflowStore, WorkflowTool};
    let manager = SubagentManager::new(1);
    let inbox = CompletionInbox::with_capacity(manager.clone(), 1);
    let (ui, mut events) = mpsc::unbounded_channel();
    let (worker, mut commands) = mpsc::unbounded_channel();
    let (done, mut completed) = mpsc::unbounded_channel();
    let sink = inbox.clone();
    let subagent = Arc::new(
        SubagentTool::with_tools(
            Arc::new(ScriptedModel::new(vec![
                ModelResponse::final_text("private"),
                ModelResponse::final_text("final"),
            ])),
            Arc::new(Vec::new),
        )
        .background(manager.clone(), move |notification| {
            let terminal = notification.spawn.run.is_none();
            sink.publish(notification, &ui, &worker);
            if terminal {
                let _ = done.send(());
            }
        }),
    );
    let workflow = WorkflowTool::new(subagent, WorkflowStore::new()).unwrap();
    workflow.call(json!({"action":"run","graph":[{"id":"a","prompt":"first"},{"id":"b","needs":["a"],"prompt":"{{ stages.a.output }}"}]}),&ToolContext {
        call_id:"workflow".into(),tool_name:"workflow".into(),cancellation:CancellationToken::new(),deadline:None,
    }).await.unwrap();
    assert!(manager.active_for_parent().is_empty());
    tokio::time::timeout(std::time::Duration::from_secs(5), completed.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        commands.try_recv().unwrap(),
        WorkerCmd::BackgroundSubagentsReady
    ));
    assert!(commands.try_recv().is_err());
    let batch = inbox.drain();
    assert_eq!(batch.len(), 1);
    let prompt = completions_prompt(&batch);
    assert!(prompt.contains("final"));
    assert!(!prompt.contains("private"));
    let mut count = 0;
    while events.try_recv().is_ok() {
        count += 1;
    }
    assert_eq!(count, 3);
}

/// The full path a workflow takes in the interactive host: the parent model
/// calls the tool, the graph fans out and back in through the real manager and
/// inbox, and the delivery extension hands the parent one terminal outcome.
///
/// The design's central claim is what this asserts: no stage answer, and no
/// stage worker, ever appears in the parent's context.
#[tokio::test]
async fn whole_pipeline_delivers_one_outcome_and_no_stage_output_to_the_parent() {
    use orca_harness_core::testing::{call, ScriptedModel};
    use orca_harness_core::{Agent, CancellationToken, Message, Model, ModelError, ToolSchema};
    use orca_harness_tools::{SubagentTool, WorkflowStore, WorkflowTool};
    use std::time::Duration;

    /// Answers the prompt it is given, so a stage's text is traceable: the
    /// source returns a list, each mapped child echoes its item.
    struct Stage;
    #[async_trait]
    impl Model for Stage {
        async fn generate(
            &self,
            context: &Context,
            _: &[ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            let prompt = context
                .messages()
                .iter()
                .rev()
                .find_map(|message| match message {
                    Message::User { content, .. } => Some(content.clone()),
                    _ => None,
                })
                .unwrap();
            Ok(ModelResponse::final_text(if prompt == "dimensions" {
                "[\"bugs\",\"perf\"]".into()
            } else {
                prompt
            }))
        }
    }

    let manager = SubagentManager::new(2);
    let inbox = CompletionInbox::new(manager.clone());
    let store = WorkflowStore::new();
    let (ui, mut events) = mpsc::unbounded_channel();
    let (worker, mut commands) = mpsc::unbounded_channel();
    let (done, mut completed) = mpsc::unbounded_channel();
    let sink = inbox.clone();
    let notify_ui = ui.clone();
    let subagent = Arc::new(
        SubagentTool::with_tools(Arc::new(Stage), Arc::new(Vec::new)).background(
            manager.clone(),
            move |notification| {
                let terminal = notification.spawn.run.is_none();
                sink.publish(notification, &notify_ui, &worker);
                if terminal {
                    let _ = done.send(());
                }
            },
        ),
    );
    let graph = json!([
        {"id":"source","prompt":"dimensions","schema":"string[]"},
        {"id":"review","kind":"map","over":"source","prompt":"secret finding for {{ item }}"},
        {"id":"report","needs":["review"],"prompt":"summary of {{ stages.review.output }}"}
    ]);
    let agent = Agent::new(Arc::new(ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call(
            "run-1",
            "workflow",
            json!({"action": "run", "graph": graph}),
        )]),
        ModelResponse::final_text("Workflow submitted."),
        ModelResponse::final_text("Workflow reported."),
    ])) as Arc<dyn Model>)
    .tool_arc(Arc::new(
        WorkflowTool::new(subagent.clone(), store.clone()).unwrap(),
    ))
    .tool_arc(subagent)
    .extension(delivery(inbox.clone(), ui));

    let mut context = Context::new();
    context.push_user("review this change");
    let answer = tokio::time::timeout(
        Duration::from_secs(5),
        agent.run_context(&mut context, CancellationToken::new()),
    )
    .await
    .expect("the parent must not wait for stages")
    .unwrap();
    assert_eq!(answer, "Workflow submitted.");
    assert!(
        manager.active_for_parent().is_empty(),
        "stages are not parent-visible work"
    );

    tokio::time::timeout(Duration::from_secs(5), completed.recv())
        .await
        .expect("the run must terminate")
        .unwrap();
    assert!(matches!(
        commands.try_recv().unwrap(),
        WorkerCmd::BackgroundSubagentsReady
    ));

    let before = context.messages().len();
    context.push_user("and now?");
    let answer = tokio::time::timeout(
        Duration::from_secs(5),
        agent.run_context(&mut context, CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(answer, "Workflow reported.");

    let delivered = context.messages()[before..]
        .iter()
        .filter_map(|message| match message {
            Message::User { content, .. } => Some(content.clone()),
            _ => None,
        })
        .filter(|content| content.contains("background_subagent_completions"))
        .collect::<Vec<_>>();
    assert_eq!(delivered.len(), 1, "one terminal outcome, one delivery");
    let outcome: serde_json::Value = serde_json::from_str(&delivered[0]).unwrap();
    let workflow = &outcome["completions"][0]["outcome"]["result"]["workflow"];
    assert_eq!(
        workflow["outputs"]["report"],
        json!("summary of [\"secret finding for bugs\",\"secret finding for perf\"]")
    );
    assert_eq!(workflow["timings"]["review"]["virtual"], json!(true));
    assert_eq!(workflow["peakAdmitted"], json!(2));

    let elsewhere = context
        .messages()
        .iter()
        .map(|message| format!("{message:?}"))
        .filter(|text| !text.contains("background_subagent_completions"))
        .collect::<String>();
    assert!(
        // The parent's own submission holds the template, never a rendering.
        !elsewhere.contains("secret finding for bugs"),
        "a stage answer reaches the parent only inside the terminal outcome"
    );
    assert!(
        !elsewhere.contains("\"activeStages\""),
        "stages stay out of the parent's agent inventory"
    );
    // The terminal payload carries the outcome twice: once as the `answer`
    // string every notification consumer expects, once as structured
    // `workflow`. Both reach the parent, so this run's outcome is billed
    // twice; the assertion pins that until the payload is narrowed.
    assert_eq!(delivered[0].matches("secret finding for bugs").count(), 2);

    // Everything the run produced is in the session store, and nowhere else.
    assert_eq!(store.runs(), 1);
    store.clear();
    assert_eq!(store.runs(), 0);
    while events.try_recv().is_ok() {}
}
