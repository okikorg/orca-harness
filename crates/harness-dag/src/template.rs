use crate::GraphError;
use std::collections::{BTreeMap, BTreeSet};
fn parts(prompt: &str) -> Result<Vec<(&str, Option<&str>)>, GraphError> {
    let mut rest = prompt;
    let mut result = Vec::new();
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| GraphError("unclosed template".into()))?;
        result.push((&rest[..start], Some(after[..end].trim())));
        rest = &after[end + 2..];
    }
    result.push((rest, None));
    Ok(result)
}
fn reference(expr: &str) -> Option<&str> {
    expr.strip_prefix("stages.")?.strip_suffix(".output")
}
pub(crate) fn validate(
    prompt: &str,
    ancestors: &BTreeSet<String>,
    item: bool,
) -> Result<(), GraphError> {
    for (_, expr) in parts(prompt)? {
        if let Some(expr) = expr {
            if expr == "item" && item {
                continue;
            }
            if reference(expr).is_some_and(|id| ancestors.contains(id)) {
                continue;
            }
            return Err(GraphError(format!(
                "invalid or non-upstream template `{{{{ {expr} }}}}`"
            )));
        }
    }
    Ok(())
}
pub(crate) fn render(
    prompt: &str,
    outputs: &BTreeMap<String, String>,
    item: Option<&str>,
) -> Result<String, GraphError> {
    let mut text = String::new();
    for (literal, expr) in parts(prompt)? {
        text.push_str(literal);
        if let Some(expr) = expr {
            let value = if expr == "item" {
                item
            } else {
                reference(expr).and_then(|id| outputs.get(id).map(String::as_str))
            };
            text.push_str(
                value.ok_or_else(|| GraphError(format!("missing template value `{expr}`")))?,
            );
        }
    }
    Ok(text)
}
