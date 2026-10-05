use async_trait::async_trait;
use orca_harness_core::testing::ScriptedModel;
use orca_harness_sdk::orchestration::SubagentRequest;
use orca_harness_sdk::{
    Context, Extension, ExtensionError, Harness, Message, ModelResponse, RunRequest,
    SubagentConfig, Subscriptions,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
mod common;

struct Inject(Arc<AtomicUsize>);
#[async_trait]
impl Extension for Inject {
    fn name(&self) -> &str {
        "request-only"
    }
    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().before_model()
    }
    async fn before_model(&self, context: &mut Context) -> Result<(), ExtensionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        context.push_user("request marker");
        Ok(())
    }
}

#[tokio::test]
async fn run_extensions_affect_only_requested_run_and_are_recorded() {
    let root = common::temp_dir("run-extensions");
    let harness = Harness::builder()
        .workspace(&root)
        .state_dir(root.join("state"))
        .build()
        .unwrap();
    let model = Arc::new(ScriptedModel::new(
        (0..5).map(|_| ModelResponse::final_text("done")).collect(),
    ));
    let agent = harness
        .agent(model.clone())
        .name("scripted")
        .subagents(SubagentConfig::default())
        .build()
        .unwrap();
    let session = agent.new_session().persistent().open().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    session
        .run(RunRequest::new("first").extension(Inject(calls.clone())))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(model.observed_contexts()[0]
        .messages()
        .iter()
        .any(|m| matches!(m,Message::User{content,..} if content=="request marker")));
    session.run("later").await.unwrap();
    agent
        .new_session()
        .open()
        .unwrap()
        .run("other session")
        .await
        .unwrap();
    session
        .subagents()
        .unwrap()
        .run(SubagentRequest::new("child"), None, None)
        .await
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "request extension leaked into a later run, another session, or child"
    );
    let resumed = agent.resume_session(&session.id().unwrap()).unwrap();
    assert!(
        resumed
            .messages()
            .await
            .iter()
            .any(|m| matches!(m,Message::User{content,..} if content=="request marker")),
        "injection must participate in persistence"
    );
    resumed
        .run(RunRequest::new("shared").extension_arc(Arc::new(Inject(calls.clone()))))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let _ = std::fs::remove_dir_all(root);
}
