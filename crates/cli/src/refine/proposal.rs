//! The single-proposal contract the proposer model must emit.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptFile {
    pub path: String,
    pub code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillProposal {
    pub name: String,
    pub description: String,
    pub body: String,
    #[serde(default)]
    pub scripts: Vec<ScriptFile>,
    pub citations: Vec<String>,
}

/// What the proposer replied: a skill, or a reasoned "nothing worth
/// packaging". Forcing a skill out of every session manufactures junk
/// lessons, so declining is a first-class answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Proposal(SkillProposal),
    NoLesson(String),
}

/// Parse the proposer's reply. Weak models wrap JSON in prose or code
/// fences, so take the outermost `{...}` span rather than requiring a
/// bare object. A reply that ran out of output budget is diagnosed as
/// truncated — "resubmit smaller" repairs; "schema error" does not.
pub fn parse(text: &str) -> Result<Reply, String> {
    let start = text.find('{').ok_or("no JSON object in reply")?;
    let end = text.rfind('}').ok_or("no JSON object in reply")?;
    if end < start {
        return Err("no JSON object in reply".into());
    }
    let span = &text[start..=end];
    if let Ok(none) = serde_json::from_str::<NoLesson>(span) {
        if none.none {
            return Ok(Reply::NoLesson(none.reason.unwrap_or_default()));
        }
    }
    match serde_json::from_str(span) {
        Ok(proposal) => Ok(Reply::Proposal(proposal)),
        Err(_) if is_incomplete_json(&text[start..]) => Err(
            "reply is truncated: the output budget ran out before the JSON object closed — \
             resubmit a smaller proposal (shorter body, fewer or shorter scripts)"
                .into(),
        ),
        Err(e) => Err(format!("proposal does not match schema: {e}")),
    }
}

#[derive(Deserialize)]
struct NoLesson {
    none: bool,
    reason: Option<String>,
}

/// True when the text opens JSON that never closes: unbalanced braces
/// or an unterminated string. A balanced-but-wrong object is malformed,
/// not truncated.
fn is_incomplete_json(text: &str) -> bool {
    let mut depth = 0_i64;
    let mut in_string = false;
    let mut escaped = false;
    for c in text.chars() {
        match (in_string, escaped, c) {
            (true, true, _) => escaped = false,
            (true, false, '\\') => escaped = true,
            (true, false, '"') => in_string = false,
            (true, false, _) => {}
            (false, _, '"') => in_string = true,
            (false, _, '{' | '[') => depth += 1,
            (false, _, '}' | ']') => depth -= 1,
            _ => {}
        }
    }
    in_string || depth > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(text: &str) -> SkillProposal {
        match parse(text).unwrap() {
            Reply::Proposal(p) => p,
            Reply::NoLesson(r) => panic!("expected proposal, got NoLesson({r})"),
        }
    }

    #[test]
    fn a_declined_run_parses_as_no_lesson_with_its_reason() {
        let reply = parse("{\"none\": true, \"reason\": \"trajectory is pure Q&A\"}").unwrap();
        assert_eq!(reply, Reply::NoLesson("trajectory is pure Q&A".into()));
    }

    #[test]
    fn a_truncated_reply_is_diagnosed_as_out_of_budget_not_schema() {
        let cut = "{\"name\":\"retry-with-backoff\",\"description\":\"d\",\"body\":\"b\",\
                   \"scripts\":[{\"path\":\"scripts/a.py\",\"code\":\"print(1)\"}";
        let err = parse(cut).unwrap_err();
        assert!(err.contains("output budget"), "{err}");
        assert!(err.contains("resubmit a smaller proposal"));

        // Balanced but wrong stays a schema error.
        let wrong = "{\"name\":\"a\"}";
        assert!(parse(wrong).unwrap_err().contains("schema"));
    }

    #[test]
    fn parses_a_fenced_proposal_with_scripts() {
        let reply = "Here you go:\n```json\n{\"name\":\"retry-with-backoff\",\
                     \"description\":\"d\",\"body\":\"b\",\
                     \"scripts\":[{\"path\":\"scripts/check.py\",\"code\":\"print(1)\"}],\
                     \"citations\":[\"e03\"]}\n```";
        let p = proposal(reply);
        assert_eq!(p.name, "retry-with-backoff");
        assert_eq!(p.scripts.len(), 1);
        assert_eq!(p.scripts[0].path, "scripts/check.py");
        assert_eq!(p.citations, vec!["e03"]);
    }

    #[test]
    fn scripts_are_optional_and_schema_errors_are_reported() {
        let p = proposal("{\"name\":\"a\",\"description\":\"d\",\"body\":\"b\",\"citations\":[]}");
        assert!(p.scripts.is_empty());

        assert!(parse("no json here")
            .unwrap_err()
            .contains("no JSON object"));
        assert!(parse("{\"name\":\"a\"}").unwrap_err().contains("schema"));
    }
}
