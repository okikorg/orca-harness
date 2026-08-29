//! The one-shot proposer call: a scratch context (never the user's
//! conversation), one generate, and at most one repair retry with the
//! validator's failures fed back verbatim.

use orca_harness_core::{Context, Message, Model, ModelResponse};

use super::proposal::{parse, Reply, SkillProposal};
use super::trajectory::{events_from, proposer_prompt};
use super::validate::{repair_feedback, validate, Check};

#[derive(Debug, Clone)]
pub struct RefineOutcome {
    pub proposal: SkillProposal,
    pub checks: Vec<Check>,
    pub repaired: bool,
}

/// A failed attempt burns the whole run's tokens, so give the model a
/// real chance to fix its mistakes — parse failures included — before
/// giving up.
const REPAIR_RETRIES: usize = 2;

/// The proposer's verdict: a validated skill, or a reasoned decline.
#[derive(Debug, Clone)]
pub enum Refined {
    Skill(RefineOutcome),
    /// The trajectory held nothing worth a reusable skill; the string
    /// is the model's reason, shown to the user.
    Nothing(String),
}

/// `existing` is the installed catalog — the proposer must not
/// duplicate a lesson or reuse a name.
pub async fn run_proposer(
    model: &dyn Model,
    messages: &[Message],
    existing: &[(String, String)],
) -> Result<Refined, String> {
    let events = events_from(messages);
    if events.is_empty() {
        return Err("nothing to refine: the session has no trajectory yet".into());
    }
    let roster: Vec<String> = events.iter().map(|e| e.id.clone()).collect();
    let existing_names: Vec<String> = existing.iter().map(|(name, _)| name.clone()).collect();

    let mut scratch = Context::new();
    scratch.push_user(proposer_prompt(&events, existing));
    let mut last_failure = String::new();
    for attempt in 0..=REPAIR_RETRIES {
        let reply = generate_text(model, &scratch).await?;
        let feedback = match parse(&reply) {
            Ok(Reply::NoLesson(reason)) => return Ok(Refined::Nothing(reason)),
            Err(err) => {
                last_failure = err.clone();
                format!(
                    "Your reply could not be used: {err}\n\
                     Resubmit the single corrected JSON object and nothing else."
                )
            }
            Ok(Reply::Proposal(proposal)) => {
                let checks = validate(&proposal, &roster, &existing_names);
                if checks.iter().all(|c| c.ok) {
                    return Ok(Refined::Skill(RefineOutcome {
                        proposal,
                        checks,
                        repaired: attempt > 0,
                    }));
                }
                last_failure = checks
                    .iter()
                    .filter(|c| !c.ok)
                    .map(|c| format!("- {}", c.msg))
                    .collect::<Vec<_>>()
                    .join("\n");
                repair_feedback(&checks)
            }
        };
        scratch.push_assistant_text(reply);
        scratch.push_user(feedback);
    }
    Err(format!(
        "proposal still invalid after {REPAIR_RETRIES} repair retries:\n{last_failure}"
    ))
}

async fn generate_text(model: &dyn Model, context: &Context) -> Result<String, String> {
    match model.generate(context, &[]).await {
        Ok(ModelResponse::Final { text, .. }) => Ok(text),
        Ok(ModelResponse::ToolCalls { .. }) => {
            Err("proposer replied with tool calls instead of the JSON proposal".into())
        }
        Err(err) => Err(format!("proposer call failed: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use orca_harness_core::{ModelError, ToolSchema};
    use std::sync::Mutex;

    struct ScriptedModel {
        replies: Mutex<Vec<String>>,
        prompts: Mutex<Vec<String>>,
    }

    impl ScriptedModel {
        fn new(replies: Vec<&str>) -> Self {
            let mut replies: Vec<String> = replies.into_iter().map(String::from).collect();
            replies.reverse();
            Self {
                replies: Mutex::new(replies),
                prompts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Model for ScriptedModel {
        async fn generate(
            &self,
            context: &Context,
            _tools: &[ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            if let Some(Message::User { content, .. }) = context.messages().last() {
                self.prompts.lock().unwrap().push(content.clone());
            }
            let reply = self.replies.lock().unwrap().pop().expect("scripted reply");
            Ok(ModelResponse::final_text(reply))
        }
    }

    fn messages() -> Vec<Message> {
        vec![
            Message::User {
                content: "add retries".into(),
                images: Vec::new(),
            },
            Message::Assistant {
                content: Some("added jittered backoff".into()),
                tool_calls: Vec::new(),
            },
        ]
    }

    const GOOD: &str = "{\"name\":\"retry-with-backoff\",\"description\":\"d\",\
                        \"body\":\"b\",\"citations\":[\"e01\"]}";
    const BAD: &str = "{\"name\":\"Bad Name\",\"description\":\"d\",\
                       \"body\":\"b\",\"citations\":[\"call_x\"]}";

    fn skill(refined: Refined) -> RefineOutcome {
        match refined {
            Refined::Skill(outcome) => outcome,
            Refined::Nothing(reason) => panic!("expected a skill, got Nothing({reason})"),
        }
    }

    #[tokio::test]
    async fn a_valid_first_reply_needs_no_repair() {
        let model = ScriptedModel::new(vec![GOOD]);
        let outcome = skill(run_proposer(&model, &messages(), &[]).await.unwrap());
        assert!(!outcome.repaired);
        assert_eq!(outcome.proposal.name, "retry-with-backoff");
        assert!(outcome.checks.iter().all(|c| c.ok));
    }

    #[tokio::test]
    async fn an_invalid_reply_gets_one_repair_retry_with_failures_fed_back() {
        let model = ScriptedModel::new(vec![BAD, GOOD]);
        let outcome = skill(run_proposer(&model, &messages(), &[]).await.unwrap());
        assert!(outcome.repaired);
        assert_eq!(outcome.proposal.name, "retry-with-backoff");

        let prompts = model.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 2);
        assert!(prompts[1].contains("Bad Name"));
        assert!(prompts[1].contains("call_x"));
    }

    #[tokio::test]
    async fn exhausted_repair_retries_are_an_error_not_a_loop() {
        let model = ScriptedModel::new(vec![BAD, BAD, BAD]);
        let err = run_proposer(&model, &messages(), &[]).await.unwrap_err();
        assert!(err.contains("after 2 repair retries"));
        assert!(err.contains("Bad Name"));
    }

    #[tokio::test]
    async fn an_unparseable_reply_is_repaired_not_fatal() {
        let model = ScriptedModel::new(vec!["sure, here is my analysis with no json", GOOD]);
        let outcome = skill(run_proposer(&model, &messages(), &[]).await.unwrap());
        assert!(outcome.repaired);

        let prompts = model.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 2);
        assert!(prompts[1].contains("could not be used"));
    }

    #[tokio::test]
    async fn a_no_lesson_reply_is_a_reasoned_decline_not_an_error() {
        let model = ScriptedModel::new(vec!["{\"none\": true, \"reason\": \"pure Q&A\"}"]);
        match run_proposer(&model, &messages(), &[]).await.unwrap() {
            Refined::Nothing(reason) => assert_eq!(reason, "pure Q&A"),
            Refined::Skill(outcome) => panic!("unexpected skill {}", outcome.proposal.name),
        }
    }

    #[tokio::test]
    async fn a_name_collision_with_an_installed_skill_is_repaired() {
        const RENAMED: &str = "{\"name\":\"retry-with-jitter\",\"description\":\"d\",\
                               \"body\":\"b\",\"citations\":[\"e01\"]}";
        let existing = vec![("retry-with-backoff".to_string(), "already here".to_string())];
        let model = ScriptedModel::new(vec![GOOD, RENAMED]);
        let outcome = skill(run_proposer(&model, &messages(), &existing).await.unwrap());
        assert!(outcome.repaired);
        assert_eq!(outcome.proposal.name, "retry-with-jitter");

        let prompts = model.prompts.lock().unwrap();
        // The catalog is in the first prompt; the collision in the repair.
        assert!(prompts[0].contains("retry-with-backoff — already here"));
        assert!(prompts[1].contains("already names an installed skill"));
    }

    #[tokio::test]
    async fn an_empty_session_refuses_to_run() {
        let model = ScriptedModel::new(vec![]);
        let err = run_proposer(&model, &[], &[]).await.unwrap_err();
        assert!(err.contains("nothing to refine"));
    }
}
