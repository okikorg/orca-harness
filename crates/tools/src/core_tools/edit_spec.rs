//! Pure, zero-copy validation and parsing for `edit_file` operations.

use orca_harness_core::ToolError;
use serde_json::Value;

pub(super) const MAX_OPERATIONS: usize = 256;

#[derive(Debug)]
pub(super) struct EditSpec {
    pub(super) path: String,
    pub(super) operation: EditOperation,
}

#[derive(Debug)]
pub(super) enum EditOperation {
    Replace {
        old: String,
        new: String,
        replace_all: bool,
    },
    Append {
        content: String,
    },
}

enum BorrowedOperation<'a> {
    Replace {
        old: &'a str,
        new: &'a str,
        replace_all: bool,
    },
    Append {
        content: &'a str,
    },
}

struct BorrowedSpec<'a> {
    path: &'a str,
    operation: BorrowedOperation<'a>,
}

/// The call's edits: the `edits` batch, or the call itself as a single
/// `{path, old, new}` edit.
pub(super) fn edits(input: &Value) -> Result<&[Value], ToolError> {
    let edits = match input.get("edits") {
        Some(edits) => edits
            .as_array()
            .ok_or_else(|| ToolError::msg("`edits` must be an array"))?,
        None if input.get("path").is_some() => return Ok(std::slice::from_ref(input)),
        None => {
            return Err(ToolError::msg(
                "pass `path`, `old`, and `new`, or an `edits` array",
            ))
        }
    };
    if edits.is_empty() {
        return Err(ToolError::msg("`edits` must contain at least one edit"));
    }
    if edits.len() > MAX_OPERATIONS {
        return Err(ToolError::msg(format!(
            "`edits` exceeds the {MAX_OPERATIONS}-operation limit"
        )));
    }
    Ok(edits)
}

fn parse_spec<'a>(at: &str, edit: &'a Value) -> Result<BorrowedSpec<'a>, ToolError> {
    let string = |key| {
        edit.get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg(format!("`{at}{key}` (string) is required")))
    };
    let operation = edit
        .get("operation")
        .map(|value| {
            value.as_str().ok_or_else(|| {
                ToolError::msg(format!("`{at}operation` must be `replace` or `append`"))
            })
        })
        .transpose()?
        .unwrap_or("replace");
    let operation = match operation {
        "replace" => {
            let old = string("old")?;
            if old.is_empty() {
                return Err(ToolError::msg(format!(
                    "`{at}old` must not be empty for replacement; use `operation: \"append\"` with `content`, or write_file for a new file"
                )));
            }
            BorrowedOperation::Replace {
                old,
                new: string("new")?,
                replace_all: edit
                    .get("replaceAll")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }
        }
        "append" => {
            let content = string("content")?;
            if content.is_empty() {
                return Err(ToolError::msg(format!(
                    "`{at}content` must not be empty for append"
                )));
            }
            BorrowedOperation::Append { content }
        }
        other => {
            return Err(ToolError::msg(format!(
                "`{at}operation` must be `replace` or `append`, got `{other}`"
            )))
        }
    };
    Ok(BorrowedSpec {
        path: string("path")?,
        operation,
    })
}

/// Each edit with the field prefix its errors name: `edits[i].` in a
/// batch, nothing for a single top-level edit.
fn located(input: &Value) -> Result<impl Iterator<Item = (String, &Value)>, ToolError> {
    let batch = input.get("edits").is_some();
    Ok(edits(input)?.iter().enumerate().map(move |(index, edit)| {
        let at = if batch {
            format!("edits[{index}].")
        } else {
            String::new()
        };
        (at, edit)
    }))
}

pub(super) fn validate(input: &Value) -> Result<(), ToolError> {
    for (at, edit) in located(input)? {
        parse_spec(&at, edit)?;
    }
    Ok(())
}

pub(super) fn parse(input: &Value) -> Result<Vec<EditSpec>, ToolError> {
    located(input)?
        .map(|(at, edit)| {
            let spec = parse_spec(&at, edit)?;
            let operation = match spec.operation {
                BorrowedOperation::Replace {
                    old,
                    new,
                    replace_all,
                } => EditOperation::Replace {
                    old: old.to_owned(),
                    new: new.to_owned(),
                    replace_all,
                },
                BorrowedOperation::Append { content } => EditOperation::Append {
                    content: content.to_owned(),
                },
            };
            Ok(EditSpec {
                path: spec.path.to_owned(),
                operation,
            })
        })
        .collect()
}
