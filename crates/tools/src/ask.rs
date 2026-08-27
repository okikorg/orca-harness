//! `ask` — pause an agent run for structured clarification from its user.
//!
//! The tool owns no UI. A host supplies a callback that receives an
//! [`AskRequest`] and eventually answers its one-shot sender.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use orca_harness_core::{Concurrency, Tool, ToolContext, ToolError, ToolSchema};

pub const MAX_ASK_TOPICS: usize = 3;
const MAX_QUESTIONS_PER_TOPIC: usize = 6;
const MAX_OPTIONS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskOption {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskQuestion {
    #[serde(default)]
    pub id: String,
    pub question: String,
    #[serde(default)]
    pub options: Vec<AskOption>,
    #[serde(default)]
    pub multiple: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskTopic {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub questions: Vec<AskQuestion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskAnswer {
    pub question_id: String,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskTopicAnswer {
    pub id: String,
    pub answers: Vec<AskAnswer>,
    /// Optional user-authored input shown by default for every topic.
    pub additional_context: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskResponse {
    Answered(Vec<AskTopicAnswer>),
    Cancelled,
}

/// A host-visible question set. Dropping `respond` dismisses the request.
pub struct AskRequest {
    pub call_id: String,
    pub topics: Vec<AskTopic>,
    pub respond: oneshot::Sender<AskResponse>,
}

pub struct AskTool {
    present: Arc<dyn Fn(AskRequest) -> Result<(), AskRequest> + Send + Sync>,
}

impl AskTool {
    pub fn new<F>(present: F) -> Self
    where
        F: Fn(AskRequest) -> Result<(), AskRequest> + Send + Sync + 'static,
    {
        Self {
            present: Arc::new(present),
        }
    }
}

#[async_trait]
impl Tool for AskTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "ask".into(),
            description: "Ask up to 3 focused multiple-choice requirement questions. Use the simple flat format: one topic, one question, and a list of choices per item. Explain each choice directly in its option string, for example `Test-first — write tests before implementation`. The UI also gives the user an Other text field. Ask only when the answer changes the work."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "topics": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_ASK_TOPICS,
                        "description": "1 to 3 multiple-choice questions.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "topic": {
                                    "type": "string",
                                    "description": "Short tab label, for example Development approach."
                                },
                                "question": {
                                    "type": "string",
                                    "description": "The requirement question to ask."
                                },
                                "options": {
                                    "type": "array",
                                    "minItems": 2,
                                    "maxItems": MAX_OPTIONS,
                                    "description": "Choice strings in `Label — explanation` form.",
                                    "items": { "type": "string" }
                                },
                                "multiple": {
                                    "type": "boolean",
                                    "description": "Allow more than one choice. Defaults to false.",
                                    "default": false
                                }
                            },
                            "required": ["question", "options"]
                        }
                    }
                },
                "required": ["topics"]
            }),
        }
    }

    fn concurrency(&self, _input: &Value) -> Concurrency {
        Concurrency::Serial
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let topics = parse_topics(input)?;
        let (respond, answer) = oneshot::channel();
        let request = AskRequest {
            call_id: ctx.call_id.clone(),
            topics,
            respond,
        };
        if (self.present)(request).is_err() {
            return Err(ToolError::msg("interactive question host is unavailable"));
        }
        tokio::select! {
            _ = ctx.cancellation.cancelled() => Err(ToolError::msg("question cancelled")),
            answer = answer => match answer {
                Ok(AskResponse::Answered(topics)) => Ok(json!({ "topics": topics })),
                Ok(AskResponse::Cancelled) => Err(ToolError::msg("question cancelled by user")),
                Err(_) => Err(ToolError::msg("interactive question was dismissed")),
            }
        }
    }
}

fn clean(value: &mut String) {
    *value = value.trim().to_string();
}

fn generated_id(prefix: &str, index: usize) -> String {
    format!("{prefix}_{}", index + 1)
}

fn humanize_id(id: &str) -> String {
    let words = id
        .split(['_', '-'])
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn split_option(text: &str) -> (String, String) {
    let text = text.trim();
    for separator in [" — ", " - ", ": "] {
        if let Some((label, description)) = text.split_once(separator) {
            if !label.trim().is_empty() && !description.trim().is_empty() {
                return (label.trim().to_string(), description.trim().to_string());
            }
        }
    }
    (text.to_string(), format!("Choose {text}"))
}

/// Accept the advertised flat shape and the older nested shape. This is
/// intentionally forgiving: tool calls should fail only when their meaning
/// is genuinely ambiguous, not because a small model forgot bookkeeping.
fn normalize_input(mut input: Value) -> Value {
    let Some(topics) = input.get_mut("topics").and_then(Value::as_array_mut) else {
        return input;
    };
    for (topic_index, topic) in topics.iter_mut().enumerate() {
        let Some(object) = topic.as_object_mut() else {
            continue;
        };
        if !object.contains_key("title") {
            if let Some(title) = object.get("topic").or_else(|| object.get("name")).cloned() {
                object.insert("title".into(), title);
            }
        }
        if !object.contains_key("id") {
            object.insert(
                "id".into(),
                Value::String(generated_id("topic", topic_index)),
            );
        }
        if !object.contains_key("questions") {
            let question = object.remove("question").unwrap_or(Value::Null);
            let options = object
                .remove("options")
                .or_else(|| object.remove("choices"))
                .unwrap_or_else(|| Value::Array(Vec::new()));
            let multiple = object.remove("multiple").unwrap_or(Value::Bool(false));
            object.insert(
                "questions".into(),
                json!([{
                    "question": question,
                    "options": options,
                    "multiple": multiple
                }]),
            );
        }
        if let Some(questions) = object.get_mut("questions").and_then(Value::as_array_mut) {
            for question in questions {
                let Some(question) = question.as_object_mut() else {
                    continue;
                };
                if !question.contains_key("options") {
                    if let Some(choices) = question.remove("choices") {
                        question.insert("options".into(), choices);
                    }
                }
                if let Some(options) = question.get_mut("options").and_then(Value::as_array_mut) {
                    for option in options {
                        if let Some(text) = option.as_str() {
                            let (label, description) = split_option(text);
                            *option = json!({"label": label, "description": description});
                        } else if let Some(option) = option.as_object_mut() {
                            if !option.contains_key("description") {
                                if let Some(label) = option.get("label").and_then(Value::as_str) {
                                    option.insert(
                                        "description".into(),
                                        Value::String(format!("Choose {label}")),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    input
}

fn parse_topics(input: Value) -> Result<Vec<AskTopic>, ToolError> {
    let input = normalize_input(input);
    let mut topics: Vec<AskTopic> =
        serde_json::from_value(input.get("topics").cloned().unwrap_or(Value::Null))
            .map_err(|err| ToolError::msg(format!("invalid `topics`: {err}")))?;
    if topics.is_empty() {
        return Err(ToolError::msg("`topics` must contain at least one topic"));
    }
    if topics.len() > MAX_ASK_TOPICS {
        return Err(ToolError::msg(
            "`ask` accepts at most 3 topics; combine or prioritize related topics",
        ));
    }
    let mut topic_ids = HashSet::new();
    for (topic_index, topic) in topics.iter_mut().enumerate() {
        clean(&mut topic.id);
        clean(&mut topic.title);
        if topic.id.is_empty() {
            topic.id = generated_id("topic", topic_index);
        }
        if topic.title.is_empty() {
            topic.title = humanize_id(&topic.id);
        }
        if !topic_ids.insert(topic.id.clone()) {
            return Err(ToolError::msg(format!(
                "topics[{topic_index}] duplicates topic id {:?}",
                topic.id
            )));
        }
        if topic.questions.is_empty() || topic.questions.len() > MAX_QUESTIONS_PER_TOPIC {
            return Err(ToolError::msg(format!(
                "topics[{topic_index}] must contain 1 to {MAX_QUESTIONS_PER_TOPIC} questions"
            )));
        }
        let mut question_ids = HashSet::new();
        for (question_index, question) in topic.questions.iter_mut().enumerate() {
            clean(&mut question.id);
            clean(&mut question.question);
            if question.id.is_empty() {
                question.id = generated_id("question", question_index);
            }
            if question.question.is_empty() || !question_ids.insert(question.id.clone()) {
                return Err(ToolError::msg(format!("topics[{topic_index}].questions[{question_index}] needs a question and a unique id")));
            }
            if !(2..=MAX_OPTIONS).contains(&question.options.len()) {
                return Err(ToolError::msg(format!("topics[{topic_index}].questions[{question_index}] must contain 2 to {MAX_OPTIONS} explained options")));
            }
            let mut labels = HashSet::new();
            for option in &mut question.options {
                clean(&mut option.label);
                option.description = option
                    .description
                    .take()
                    .map(|mut value| {
                        clean(&mut value);
                        value
                    })
                    .filter(|value| !value.is_empty());
                if option.description.as_deref().is_none_or(str::is_empty) {
                    return Err(ToolError::msg(format!("topics[{topic_index}].questions[{question_index}] option {:?} needs a useful description", option.label)));
                }
                if option.label.is_empty() || !labels.insert(option.label.clone()) {
                    return Err(ToolError::msg(format!("topics[{topic_index}].questions[{question_index}] has a blank or duplicate option label")));
                }
            }
        }
    }
    Ok(topics)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_harness_core::CancellationToken;
    use std::sync::Mutex;

    fn ctx(cancel: CancellationToken) -> ToolContext {
        ToolContext {
            call_id: "ask-1".into(),
            tool_name: "ask".into(),
            cancellation: cancel,
            deadline: None,
        }
    }

    #[tokio::test]
    async fn waits_and_returns_topic_answers() {
        let slot = Arc::new(Mutex::new(None));
        let capture = slot.clone();
        let tool = AskTool::new(move |request| {
            *capture.lock().unwrap() = Some(request);
            Ok(())
        });
        let cancel = CancellationToken::new();
        let call = tokio::spawn(async move {
            tool.call(json!({"topics":[{"id":"auth","title":"Authentication","questions":[{"id":"method","question":"Which method?","options":[{"label":"OAuth","description":"Use an external identity provider"},{"label":"Password","description":"Manage credentials in the application"}]}]}]}), &ctx(cancel)).await
        });
        tokio::task::yield_now().await;
        let request = slot.lock().unwrap().take().expect("request");
        assert_eq!(request.topics[0].title, "Authentication");
        request
            .respond
            .send(AskResponse::Answered(vec![AskTopicAnswer {
                id: "auth".into(),
                answers: vec![AskAnswer {
                    question_id: "method".into(),
                    values: vec!["OAuth".into()],
                }],
                additional_context: "Use our existing tenant".into(),
            }]))
            .unwrap();
        let output = call.await.unwrap().unwrap();
        assert_eq!(
            output["topics"][0]["additional_context"],
            "Use our existing tenant"
        );
    }

    #[test]
    fn omitted_ids_and_title_are_generated() {
        let topics = parse_topics(json!({
            "topics": [{
                "id": "dev_setup",
                "questions": [{
                    "question": "How should tests guide implementation?",
                    "options": [
                        {"label": "Test-first", "description": "Write a failing test before implementation"},
                        {"label": "Test-after", "description": "Implement first, then cover the behavior"}
                    ]
                }]
            }]
        }))
        .unwrap();
        assert_eq!(topics[0].title, "Dev setup");
        assert_eq!(topics[0].questions[0].id, "question_1");
    }

    #[test]
    fn flat_format_is_normalized_to_the_ui_model() {
        let topics = parse_topics(json!({
            "topics": [{
                "topic": "Development approach",
                "question": "How should tests guide implementation?",
                "options": [
                    "Test-first — write tests before implementation",
                    "Test-after — implement first, then add coverage"
                ]
            }]
        }))
        .unwrap();
        assert_eq!(topics[0].title, "Development approach");
        assert_eq!(topics[0].questions[0].id, "question_1");
        assert_eq!(topics[0].questions[0].options[0].label, "Test-first");
        assert_eq!(
            topics[0].questions[0].options[0].description.as_deref(),
            Some("write tests before implementation")
        );
    }

    #[test]
    fn schema_advertises_only_the_flat_format() {
        let schema = AskTool::new(|_request| Ok(())).schema();
        let item = &schema.parameters["properties"]["topics"]["items"];
        assert_eq!(item["required"], json!(["question", "options"]));
        assert_eq!(item["properties"]["options"]["items"]["type"], "string");
        assert!(item["properties"].get("questions").is_none());
    }

    #[tokio::test]
    async fn rejects_more_than_three_topics() {
        let tool = AskTool::new(|_request| Ok(()));
        let topics: Vec<Value> = (0..4).map(|i| json!({"id":format!("t{i}"),"title":format!("T{i}"),"questions":[{"id":"q","question":"Question?","options":[{"label":"A","description":"Choose A"},{"label":"B","description":"Choose B"}]}]})).collect();
        let err = tool
            .call(json!({"topics":topics}), &ctx(CancellationToken::new()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("at most 3 topics"));
    }

    #[test]
    fn bare_option_labels_are_accepted_with_fallback_explanations() {
        let topics = parse_topics(json!({
            "topics": [{
                "question": "Which release?",
                "options": ["Beta", "General availability"]
            }]
        }))
        .unwrap();
        assert_eq!(topics[0].questions[0].options[0].label, "Beta");
        assert_eq!(
            topics[0].questions[0].options[0].description.as_deref(),
            Some("Choose Beta")
        );
    }

    #[test]
    fn clarification_is_serial() {
        assert_eq!(
            AskTool::new(|_request| Ok(())).concurrency(&json!({})),
            Concurrency::Serial
        );
    }
}
