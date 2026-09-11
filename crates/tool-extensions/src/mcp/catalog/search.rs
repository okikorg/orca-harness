//! Relevance ranking for `mcp_search_tools`: tokenizing, plural folding,
//! and the exact-then-partial scoring over one tool's search fields.

pub(super) struct SearchFields {
    pub(super) name: Vec<String>,
    pub(super) description: Vec<String>,
    pub(super) server: Vec<String>,
    pub(super) parameters: Vec<String>,
}

pub(super) fn tokens(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

pub(super) fn singular(token: &str) -> String {
    if token.len() > 4 && token.ends_with("ies") {
        return format!("{}y", &token[..token.len() - 3]);
    }
    for suffix in ["ches", "shes", "sses", "xes", "zes"] {
        if token.len() > suffix.len() && token.ends_with(suffix) {
            return token[..token.len() - 2].to_owned();
        }
    }
    if token.len() > 3
        && token.ends_with('s')
        && !token.ends_with("ss")
        && !token.ends_with("us")
        && !token.ends_with("is")
    {
        return token[..token.len() - 1].to_owned();
    }
    token.to_owned()
}

fn term_match(term: &str, field: &[String], normalize_plural: bool) -> u64 {
    if field.iter().any(|token| token == term) {
        3
    } else if field.iter().any(|token| token.contains(term)) {
        2
    } else if normalize_plural && field.iter().any(|token| singular(token) == singular(term)) {
        1
    } else {
        0
    }
}

pub(super) fn relevance(terms: &[String], fields: &SearchFields) -> Option<u64> {
    let (matched, mut score) = match_score(terms, fields);
    if matched != terms.len() {
        return None;
    }

    let normalized_terms: Vec<String> = terms.iter().map(|term| singular(term)).collect();
    let normalized_name: Vec<String> = fields.name.iter().map(|term| singular(term)).collect();
    if normalized_terms == normalized_name {
        score += 100;
    } else if normalized_terms
        .iter()
        .all(|term| normalized_name.contains(term))
    {
        score += 20;
    }
    Some(score)
}

/// Natural-language searches often add words absent from compact MCP schemas.
/// Use this only when no tool matched every term, preserving precise results
/// while keeping one unmatched adjective from hiding the whole catalog.
pub(super) fn partial_relevance(terms: &[String], fields: &SearchFields) -> Option<u64> {
    let (matched, mut score) = match_score(terms, fields);
    if matched == 0 {
        return None;
    }
    score += (matched as u64 * 100) / terms.len() as u64;

    let normalized_terms: Vec<String> = terms.iter().map(|term| singular(term)).collect();
    let normalized_name: Vec<String> = fields.name.iter().map(|term| singular(term)).collect();
    if normalized_name
        .iter()
        .all(|term| normalized_terms.contains(term))
    {
        score += 20;
    }
    Some(score)
}

fn match_score(terms: &[String], fields: &SearchFields) -> (usize, u64) {
    terms.iter().fold((0, 0), |(matched, score), term| {
        let best = [
            term_match(term, &fields.name, true) * 8,
            term_match(term, &fields.description, false) * 4,
            term_match(term, &fields.server, false) * 2,
            term_match(term, &fields.parameters, false),
        ]
        .into_iter()
        .max()
        .unwrap_or_default();
        (matched + usize::from(best > 0), score + best)
    })
}
