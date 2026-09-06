use super::*;
use std::fs as stdfs;
use std::path::PathBuf;

use serde_json::json;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{Agent, CancellationToken, ModelResponse, ToolResult};

use crate::skills::tool::SkillTool;

struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "orca-skillonce-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = stdfs::remove_dir_all(&dir);
        stdfs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn write(&self, rel: &str, body: &str) {
        let path = self.0.join(rel);
        stdfs::create_dir_all(path.parent().unwrap()).unwrap();
        stdfs::write(&path, body).unwrap();
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = stdfs::remove_dir_all(&self.0);
    }
}

fn agent(temp: &Temp, model: ScriptedModel) -> Agent<ScriptedModel> {
    temp.write(
        "repo/.orca/skills/release/SKILL.md",
        "---\nname: release\ndescription: Cut a release\n---\n\n1. bump\n2. tag\n",
    );
    temp.write("repo/.orca/skills/release/checklist.md", "- green CI\n");
    let found = crate::skills::skill::discover(&crate::skills::skill::roots(
        &temp.0.join("repo"),
        None,
        None,
    ));
    Agent::new(model)
        .extension(SkillOnce::new())
        .tool(SkillTool::new(found.skills))
}

fn tool_calls(calls: Vec<ToolCall>) -> ModelResponse {
    ModelResponse::ToolCalls {
        content: None,
        calls,
        usage: None,
    }
}

fn load(id: &str) -> ToolCall {
    call(id, "skill", json!({"name": "release"}))
}

/// Every `skill` result in the transcript, in order.
fn skill_results(context: &Context) -> Vec<ToolResult> {
    context
        .messages()
        .iter()
        .filter_map(|message| match message {
            Message::Tool { results } => Some(results.iter().cloned()),
            _ => None,
        })
        .flatten()
        .filter(|result| result.tool_name == "skill")
        .collect()
}

fn is_stub(result: &ToolResult) -> bool {
    result.output.get("alreadyLoaded") == Some(&json!(true))
        && result.output.get("instructions").is_none()
}

async fn run(agent: &Agent<ScriptedModel>, context: &mut Context) {
    context.push_user("go");
    agent
        .run_context(context, CancellationToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_repeat_load_returns_a_reminder_not_the_text() {
    let temp = Temp::new("repeat");
    let agent = agent(
        &temp,
        ScriptedModel::new(vec![
            tool_calls(vec![load("1")]),
            tool_calls(vec![load("2")]),
            ModelResponse::final_text("done"),
        ]),
    );
    let mut context = Context::new();
    run(&agent, &mut context).await;

    let results = skill_results(&context);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].output["instructions"], "1. bump\n2. tag\n");
    assert!(is_stub(&results[1]), "{:?}", results[1].output);
    assert_eq!(results[1].output["name"], "release");
    let note = results[1].output["note"].as_str().unwrap();
    assert!(note.contains("already loaded"), "{note}");
    assert!(note.contains("still apply"), "{note}");
}

/// The dedupe is for the body only: a file beside it and a later page
/// of a long body are new text the model has not seen.
#[tokio::test]
async fn resource_reads_and_continuation_pages_are_not_repeats() {
    let temp = Temp::new("pages");
    let agent = agent(
        &temp,
        ScriptedModel::new(vec![
            tool_calls(vec![load("1")]),
            tool_calls(vec![
                call(
                    "2",
                    "skill",
                    json!({"name": "release", "resource": "checklist.md"}),
                ),
                call("3", "skill", json!({"name": "release", "offset": 8})),
            ]),
            ModelResponse::final_text("done"),
        ]),
    );
    let mut context = Context::new();
    run(&agent, &mut context).await;

    let results = skill_results(&context);
    assert_eq!(results.len(), 3);
    for result in &results {
        assert!(!is_stub(result), "{:?}", result.output);
        assert!(result.output["instructions"].is_string());
    }
    assert_eq!(results[1].output["instructions"], "- green CI\n");
    assert_eq!(results[2].output["instructions"], "2. tag\n");
}

/// Compaction stubs old tool results out of the context, so the text is
/// gone for real; the set is read off the context, so the next load is
/// real too. Nothing needs to be told a compaction happened.
#[tokio::test]
async fn an_elided_result_no_longer_counts_as_loaded() {
    let temp = Temp::new("elided");
    let agent = agent(
        &temp,
        ScriptedModel::new(vec![
            tool_calls(vec![load("2")]),
            ModelResponse::final_text("done"),
        ]),
    );
    let mut context = Context::new();
    context.push_user("earlier");
    context.push(Message::Assistant {
        content: None,
        tool_calls: vec![load("1")],
    });
    context.push(Message::Tool {
        results: vec![ToolResult::ok(
            &load("1"),
            json!({"_elided": true, "hint": "{\"name\":\"release\",\"instructions\":\"1. bu"}),
        )],
    });
    run(&agent, &mut context).await;

    let results = skill_results(&context);
    assert_eq!(results.len(), 2);
    assert!(!is_stub(&results[1]), "{:?}", results[1].output);
    assert_eq!(results[1].output["instructions"], "1. bump\n2. tag\n");
}

/// A `/clear` is the same story with an empty context.
#[tokio::test]
async fn a_body_already_in_the_context_is_not_reloaded() {
    let temp = Temp::new("prior");
    let agent = agent(
        &temp,
        ScriptedModel::new(vec![
            tool_calls(vec![load("2")]),
            ModelResponse::final_text("done"),
        ]),
    );
    let mut context = Context::new();
    context.push_user("earlier");
    context.push(Message::Assistant {
        content: None,
        tool_calls: vec![load("1")],
    });
    context.push(Message::Tool {
        results: vec![ToolResult::ok(
            &load("1"),
            json!({"name": "release", "instructions": "1. bump\n2. tag\n"}),
        )],
    });
    run(&agent, &mut context).await;

    let results = skill_results(&context);
    assert_eq!(results.len(), 2);
    assert!(is_stub(&results[1]), "{:?}", results[1].output);
}

#[tokio::test]
async fn two_calls_for_one_skill_in_a_batch_load_once() {
    let temp = Temp::new("batch");
    let agent = agent(
        &temp,
        ScriptedModel::new(vec![
            tool_calls(vec![load("1"), load("2")]),
            ModelResponse::final_text("done"),
        ]),
    );
    let mut context = Context::new();
    run(&agent, &mut context).await;

    let results = skill_results(&context);
    assert_eq!(results.len(), 2);
    let stubs = results.iter().filter(|result| is_stub(result)).count();
    assert_eq!(stubs, 1, "{:?}", results);
}

/// An error puts no instructions in context, so it must not be
/// remembered as a load: the model's corrected retry gets the text.
#[tokio::test]
async fn a_failed_load_is_not_remembered() {
    let temp = Temp::new("failed");
    let agent = agent(
        &temp,
        ScriptedModel::new(vec![
            tool_calls(vec![call("1", "skill", json!({"name": "nope"}))]),
            tool_calls(vec![call("2", "skill", json!({"name": "nope"}))]),
            ModelResponse::final_text("done"),
        ]),
    );
    let mut context = Context::new();
    run(&agent, &mut context).await;

    let results = skill_results(&context);
    assert_eq!(results.len(), 2);
    assert!(results[0].is_error);
    assert!(results[1].is_error, "{:?}", results[1].output);
    assert!(!is_stub(&results[1]));
}

#[test]
fn body_requests_are_the_plain_name_only() {
    assert_eq!(body_request(&json!({"name": "a"})), Some("a"));
    assert_eq!(
        body_request(&json!({"name": "a", "resource": null, "offset": null})),
        Some("a")
    );
    assert_eq!(
        body_request(&json!({"name": "a", "resource": "", "offset": 0})),
        Some("a")
    );
    assert_eq!(
        body_request(&json!({"name": "a", "resource": "x.md"})),
        None
    );
    assert_eq!(body_request(&json!({"name": "a", "offset": 12})), None);
    assert_eq!(body_request(&json!({})), None);
}

#[test]
fn loaded_bodies_carry_instructions_and_no_resource() {
    assert_eq!(
        loaded_body(&json!({"name": "a", "instructions": "text"})),
        Some("a")
    );
    assert_eq!(
        loaded_body(&json!({"name": "a", "resource": "x.md", "instructions": "text"})),
        None
    );
    assert_eq!(loaded_body(&json!({"_elided": true, "hint": "..."})), None);
    assert_eq!(loaded_body(&SkillOnce::stub("a")), None);
}
