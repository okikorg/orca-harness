//! `todo_write` — the agent's plan for the task, as structured state
//! instead of prose it has to keep re-deriving from the transcript.
//!
//! The tool takes the **whole** list every time and replaces what it
//! held. Incremental edits would need stable ids, and an id the model
//! has to remember is an id it will get wrong; re-sending five short
//! strings costs less than the bookkeeping does. A write is therefore
//! also how an item is completed, reworded, dropped, or reordered.
//!
//! The list is model-visible (it comes back as the tool result) and
//! host-visible (through the shared [`TodoList`] handle), so a UI can
//! render the current plan without parsing anything out of the
//! conversation.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use orca_harness_core::{Concurrency, Tool, ToolContext, ToolError, ToolSchema};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    pub fn label(self) -> &'static str {
        match self {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Completed => "completed",
        }
    }

    fn from_label(label: &str) -> Option<Self> {
        match label.trim().to_ascii_lowercase().as_str() {
            "pending" | "todo" | "open" => Some(TodoStatus::Pending),
            "in_progress" | "in-progress" | "active" | "doing" => Some(TodoStatus::InProgress),
            "completed" | "complete" | "done" => Some(TodoStatus::Completed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

/// Cloneable handle onto the current plan. The tool writes it; hosts read
/// it to render one.
#[derive(Clone, Default)]
pub struct TodoList(Arc<RwLock<Vec<TodoItem>>>);

impl TodoList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn items(&self) -> Vec<TodoItem> {
        self.0.read().expect("todo lock").clone()
    }

    pub fn is_empty(&self) -> bool {
        self.0.read().expect("todo lock").is_empty()
    }

    /// (completed, total) — what a status line needs.
    pub fn progress(&self) -> (usize, usize) {
        let items = self.0.read().expect("todo lock");
        let done = items
            .iter()
            .filter(|item| item.status == TodoStatus::Completed)
            .count();
        (done, items.len())
    }

    /// The item being worked on right now, if any.
    pub fn current(&self) -> Option<TodoItem> {
        self.0
            .read()
            .expect("todo lock")
            .iter()
            .find(|item| item.status == TodoStatus::InProgress)
            .cloned()
    }

    fn replace(&self, items: Vec<TodoItem>) {
        *self.0.write().expect("todo lock") = items;
    }

    /// Drop the plan entirely (the host's conversation reset).
    pub fn clear(&self) {
        self.replace(Vec::new());
    }
}

/// `todo_write` — replace the task list.
pub struct TodoWriteTool {
    list: TodoList,
}

impl TodoWriteTool {
    pub fn new(list: TodoList) -> Self {
        Self { list }
    }

    /// The shared handle this tool writes to.
    pub fn list(&self) -> TodoList {
        self.list.clone()
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "todo_write".into(),
            description: "Record the task list for the work in progress, replacing the previous \
                one. The default is to NOT use this tool. Use it only when the user explicitly \
                asks for a todo plan, or for complex, ambiguous, or multi-phase work that will \
                take 5+ distinct steps you could lose track of. Never for routine follow-ups, \
                simple requests, or single-step work — if you can hold the plan in your head or \
                finish inside a couple of tool batches, skip this tool and just do the work; \
                writing a todo list for a small task costs more than it saves. Send the complete \
                list every time: marking an item done, adding a discovered step, or dropping an \
                unnecessary one are all just another write. Keep exactly one item in_progress \
                while working."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The complete task list, in the order you will do them.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {
                                    "type": "string",
                                    "description": "The step, as an imperative phrase."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "default": "pending"
                                }
                            },
                            "required": ["content"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        }
    }

    /// One list, one writer at a time: concurrent writes in the same
    /// batch would race to be last, and last is the whole list.
    fn concurrency(&self, _input: &Value) -> Concurrency {
        Concurrency::Keyed("todo".into())
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let items = parse_todos(&input)?;
        self.list.replace(items.clone());
        Ok(render(&items))
    }
}

/// Parse and validate the `todos` argument. Lenient about spelling (a
/// model that writes `done` means `completed`), strict about the one
/// invariant that makes the list worth keeping: at most one item is in
/// progress.
fn parse_todos(input: &Value) -> Result<Vec<TodoItem>, ToolError> {
    let todos = input
        .get("todos")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::msg("`todos` (array) is required"))?;
    let mut items = Vec::with_capacity(todos.len());
    for (index, raw) in todos.iter().enumerate() {
        let content = raw
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|content| !content.is_empty())
            .ok_or_else(|| {
                ToolError::msg(format!(
                    "todos[{index}]: `content` (non-empty string) is required"
                ))
            })?;
        let status = match raw.get("status") {
            None | Some(Value::Null) => TodoStatus::Pending,
            Some(Value::String(label)) => TodoStatus::from_label(label).ok_or_else(|| {
                ToolError::msg(format!(
                    "todos[{index}]: unknown status {label:?} \
                     (expected pending, in_progress, or completed)"
                ))
            })?,
            Some(other) => {
                return Err(ToolError::msg(format!(
                    "todos[{index}]: `status` must be a string, got {other}"
                )))
            }
        };
        items.push(TodoItem {
            content: content.to_string(),
            status,
        });
    }
    let active = items
        .iter()
        .filter(|item| item.status == TodoStatus::InProgress)
        .count();
    if active > 1 {
        return Err(ToolError::msg(format!(
            "{active} items are in_progress; exactly one step is in progress at a time"
        )));
    }
    Ok(items)
}

/// The result the model sees: the list as stored, plus the counts, so it
/// can tell at a glance what is left without re-reading the array.
fn render(items: &[TodoItem]) -> Value {
    let count = |status: TodoStatus| items.iter().filter(|item| item.status == status).count();
    json!({
        "todos": items.iter().map(|item| json!({
            "content": item.content,
            "status": item.status.label(),
        })).collect::<Vec<_>>(),
        "pending": count(TodoStatus::Pending),
        "inProgress": count(TodoStatus::InProgress),
        "completed": count(TodoStatus::Completed),
        "total": items.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_harness_core::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            call_id: "c1".into(),
            tool_name: "todo_write".into(),
            cancellation: CancellationToken::new(),
            deadline: None,
        }
    }

    async fn write(tool: &TodoWriteTool, input: Value) -> Result<Value, ToolError> {
        tool.call(input, &ctx()).await
    }

    fn tool() -> TodoWriteTool {
        TodoWriteTool::new(TodoList::new())
    }

    #[test]
    fn schema_limits_when_the_tool_should_be_used() {
        let description = tool().schema().description;
        assert!(description.contains("default is to NOT use this tool"));
        assert!(description.contains("explicitly asks for a todo plan"));
        assert!(description.contains("5+ distinct steps"));
        assert!(description.contains("Never for routine follow-ups"));
        assert!(description.contains("simple requests, or single-step work"));
    }

    #[tokio::test]
    async fn a_write_replaces_the_whole_list() {
        let tool = tool();
        let list = tool.list();
        write(
            &tool,
            json!({"todos": [
                {"content": "read the code", "status": "completed"},
                {"content": "write the fix", "status": "in_progress"},
                {"content": "run the tests"}
            ]}),
        )
        .await
        .unwrap();

        assert_eq!(list.progress(), (1, 3));
        assert_eq!(list.current().unwrap().content, "write the fix");
        assert_eq!(list.items()[2].status, TodoStatus::Pending);

        // A second write is the whole list again, not a merge.
        write(
            &tool,
            json!({"todos": [{"content": "ship", "status": "done"}]}),
        )
        .await
        .unwrap();
        let items = list.items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].content, "ship");
        assert_eq!(items[0].status, TodoStatus::Completed);
    }

    #[tokio::test]
    async fn the_result_carries_the_list_and_the_counts() {
        let tool = tool();
        let out = write(
            &tool,
            json!({"todos": [
                {"content": "a", "status": "completed"},
                {"content": "b", "status": "in_progress"},
                {"content": "c"},
                {"content": "d"}
            ]}),
        )
        .await
        .unwrap();

        assert_eq!(out["total"], 4);
        assert_eq!(out["completed"], 1);
        assert_eq!(out["inProgress"], 1);
        assert_eq!(out["pending"], 2);
        assert_eq!(out["todos"][1]["status"], "in_progress");
        assert_eq!(out["todos"][3]["content"], "d");
    }

    #[tokio::test]
    async fn an_empty_list_clears_the_plan() {
        let tool = tool();
        let list = tool.list();
        write(&tool, json!({"todos": [{"content": "a"}]}))
            .await
            .unwrap();
        assert!(!list.is_empty());
        let out = write(&tool, json!({"todos": []})).await.unwrap();
        assert!(list.is_empty());
        assert_eq!(out["total"], 0);
        assert_eq!(list.current(), None);
    }

    #[tokio::test]
    async fn two_items_in_progress_is_refused() {
        let tool = tool();
        let list = tool.list();
        write(&tool, json!({"todos": [{"content": "keep me"}]}))
            .await
            .unwrap();

        let err = write(
            &tool,
            json!({"todos": [
                {"content": "a", "status": "in_progress"},
                {"content": "b", "status": "in_progress"}
            ]}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("in_progress"), "{err}");
        // A refused write leaves the previous plan intact.
        assert_eq!(list.items().len(), 1);
        assert_eq!(list.items()[0].content, "keep me");
    }

    #[tokio::test]
    async fn malformed_input_is_refused_with_the_offending_index() {
        let tool = tool();
        assert!(write(&tool, json!({})).await.is_err());
        assert!(write(&tool, json!({"todos": "nope"})).await.is_err());

        let err = write(
            &tool,
            json!({"todos": [{"content": "ok"}, {"content": "  "}]}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("todos[1]"), "{err}");

        let err = write(
            &tool,
            json!({"todos": [{"content": "ok", "status": "blocked"}]}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("todos[0]"), "{err}");
        assert!(err.to_string().contains("blocked"), "{err}");
    }

    /// Models write `done`/`doing`/`todo` as often as the canonical
    /// spellings; accepting them costs nothing and saves a retry.
    #[tokio::test]
    async fn status_synonyms_are_accepted() {
        let tool = tool();
        let list = tool.list();
        write(
            &tool,
            json!({"todos": [
                {"content": "a", "status": "TODO"},
                {"content": "b", "status": "doing"},
                {"content": "c", "status": "Done"}
            ]}),
        )
        .await
        .unwrap();
        let statuses: Vec<TodoStatus> = list.items().into_iter().map(|item| item.status).collect();
        assert_eq!(
            statuses,
            [
                TodoStatus::Pending,
                TodoStatus::InProgress,
                TodoStatus::Completed
            ]
        );
    }

    /// Two writes in one batch must not race to be last.
    #[test]
    fn writes_serialize_against_each_other() {
        let tool = tool();
        assert_eq!(
            tool.concurrency(&json!({"todos": []})),
            Concurrency::Keyed("todo".into())
        );
    }

    #[tokio::test]
    async fn the_handle_is_shared_with_the_host() {
        let list = TodoList::new();
        let tool = TodoWriteTool::new(list.clone());
        write(
            &tool,
            json!({"todos": [{"content": "a", "status": "in_progress"}]}),
        )
        .await
        .unwrap();
        // The host's clone sees the write without going through the tool.
        assert_eq!(list.current().unwrap().content, "a");
        list.clear();
        assert!(list.is_empty());
    }
}
