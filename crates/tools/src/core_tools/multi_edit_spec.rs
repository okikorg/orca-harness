//! Pure, zero-copy validation and parsing for `multi_edit` operations.

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

fn edits(input: &Value) -> Result<&[Value], ToolError> {
    let edits = input
        .get("edits")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::msg("`edits` (array) is required"))?;
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

fn parse_spec(index: usize, edit: &Value) -> Result<BorrowedSpec<'_>, ToolError> {
    let string = |key| {
        edit.get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg(format!("`edits[{index}].{key}` (string) is required")))
    };
    let operation = edit
        .get("operation")
        .map(|value| {
            value.as_str().ok_or_else(|| {
                ToolError::msg(format!(
                    "`edits[{index}].operation` must be `replace` or `append`"
                ))
            })
        })
        .transpose()?
        .unwrap_or("replace");
    let operation = match operation {
        "replace" => {
            let old = string("old")?;
            if old.is_empty() {
                return Err(ToolError::msg(format!(
                    "`edits[{index}].old` must not be empty for replacement; use `operation: \"append\"` with `content`, or use `apply_patch`"
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
                    "`edits[{index}].content` must not be empty for append"
                )));
            }
            BorrowedOperation::Append { content }
        }
        other => {
            return Err(ToolError::msg(format!(
                "`edits[{index}].operation` must be `replace` or `append`, got `{other}`"
            )))
        }
    };
    Ok(BorrowedSpec {
        path: string("path")?,
        operation,
    })
}

pub(super) fn validate(input: &Value) -> Result<(), ToolError> {
    for (index, edit) in edits(input)?.iter().enumerate() {
        parse_spec(index, edit)?;
    }
    Ok(())
}

pub(super) fn parse(input: &Value) -> Result<Vec<EditSpec>, ToolError> {
    edits(input)?
        .iter()
        .enumerate()
        .map(|(index, edit)| {
            let spec = parse_spec(index, edit)?;
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
