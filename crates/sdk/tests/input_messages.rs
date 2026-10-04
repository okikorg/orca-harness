use orca_harness_core::testing::ScriptedModel;
use orca_harness_sdk::{Harness, Image, Message, ModelResponse, RunInputMessage, RunRequest};
mod common;

#[tokio::test]
async fn batch_preserves_user_boundaries_images_and_persistence() {
    let root = common::temp_dir("input-messages");
    let harness = Harness::builder()
        .workspace(&root)
        .state_dir(root.join("state"))
        .build()
        .unwrap();
    let model = std::sync::Arc::new(ScriptedModel::new(vec![ModelResponse::final_text("done")]));
    let agent = harness
        .agent(model.clone())
        .system_prompt("system")
        .build()
        .unwrap();
    let session = agent.new_session().persistent().open().unwrap();
    let image = Image {
        media_type: "image/png".into(),
        data: "cGl4ZWw=".into(),
    };
    let result = session
        .run(RunRequest::messages([
            RunInputMessage::new("first"),
            RunInputMessage::new("second").image(image),
        ]))
        .await
        .unwrap();
    assert_eq!(result.text, "done");
    assert_eq!(model.observed_contexts().len(), 1);
    assert_eq!(model.observed_contexts()[0].messages().len(), 3);
    assert!(
        matches!(&model.observed_contexts()[0].messages()[2], Message::User { content, images } if content == "second" && images.len() == 1 && images[0].data == "cGl4ZWw=")
    );
    let id = session.id().unwrap();
    drop(session);
    drop(agent);
    drop(harness);
    let harness = Harness::builder()
        .workspace(&root)
        .state_dir(root.join("state"))
        .build()
        .unwrap();
    let restarted_model = std::sync::Arc::new(ScriptedModel::new(vec![ModelResponse::final_text(
        "after restart",
    )]));
    let restarted_agent = harness.agent(restarted_model.clone()).build().unwrap();
    let resumed = restarted_agent.resume_session(&id).unwrap();
    let messages = resumed.messages().await;
    assert_eq!(messages.len(), 4);
    assert!(matches!(&messages[0], Message::System { content } if content == "system"));
    assert!(
        matches!(&messages[1], Message::User { content, images } if content == "first" && images.is_empty())
    );
    assert!(
        matches!(&messages[2], Message::User { content, images } if content == "second" && images.len() == 1 && images[0].media_type == "image/png" && images[0].data == "cGl4ZWw=")
    );
    assert!(matches!(&messages[3], Message::Assistant { .. }));
    resumed.run("continue after restart").await.unwrap();
    assert!(
        matches!(&restarted_model.observed_contexts()[0].messages()[2], Message::User { images, .. } if images.len() == 1 && images[0].data == "cGl4ZWw=")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn invalid_batches_do_not_change_transcript_or_call_model() {
    let root = common::temp_dir("input-message-errors");
    let harness = Harness::builder()
        .workspace(&root)
        .state_dir(root.join("state"))
        .build()
        .unwrap();
    let model = std::sync::Arc::new(ScriptedModel::new(vec![]));
    let agent = harness.agent(model.clone()).build().unwrap();
    let session = agent.new_session().open().unwrap();
    assert!(session.run(RunRequest::messages([])).await.is_err());
    let mut mixed = RunRequest::messages([RunInputMessage::new("message")]);
    mixed.prompt = "also prompt".into();
    assert!(session.run(mixed).await.is_err());
    let mixed = RunRequest::messages([RunInputMessage::new("message")]).image(Image {
        media_type: "image/png".into(),
        data: "".into(),
    });
    assert!(session.run(mixed).await.is_err());
    assert!(session
        .continue_run(RunRequest::messages([RunInputMessage::new("message")]))
        .await
        .is_err());
    assert!(session.messages().await.is_empty());
    assert!(model.observed_contexts().is_empty());
    std::fs::remove_dir_all(root).unwrap();
}
