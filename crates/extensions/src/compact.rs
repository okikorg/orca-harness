//! Fully deterministic context compaction — no model call anywhere.
//!
//! Stage 1 elides tool-result payloads into the [`TruncationStore`]
//! (recoverable via `read_tool_result`). Stage 2 replaces the
//! conversation head with a mechanical summary built from the messages
//! themselves: recent user requests verbatim, a recovery index mapping
//! every elided call id to its tool and arguments, and the last
//! assistant note — all under hard character caps, fx-style. A recent
//! tail can be kept verbatim under a token budget.
//!
//! Compaction is therefore instant, free, and trust-free: nothing is
//! paraphrased, and everything dropped is either listed verbatim or
//! recoverable by id from the store.

use serde::Serialize;
use serde_json::{json, Value};

use orca_harness_core::{Context, Message};

use crate::truncation::TruncationStore;

/// Rough bytes-per-token used for all estimates (no tokenizer in the
/// harness; provider-reported usage is the ground truth between runs).
const APPROX_BYTES_PER_TOKEN: usize = 4;
/// Flat token estimate per tool image. Base64 length says little about
/// what a provider bills for an image (roughly width x height / 750,
/// capped near this), so image data counts at this rate instead.
const APPROX_IMAGE_TOKENS: usize = 1_600;

/// Per-result serialized-output size below which stage 1 leaves a tool
/// result alone: eliding tiny outputs saves nothing and costs a stub.
const ELIDE_MIN_BYTES: usize = 256;

// Hard caps on the mechanical summary. The summary is the compaction
// floor, so every constant here directly bounds the post-compact size.
const SUMMARY_REQUESTS: usize = 3;
const REQUEST_CHARS: usize = 72;
const NOTE_CHARS: usize = 100;
const INDEX_ENTRIES: usize = 8;
const INDEX_ARG_CHARS: usize = 32;

pub struct CompactConfig {
    /// Approximate token budget for the verbatim tail kept after the
    /// summary. The cut lands on a user/assistant boundary, never
    /// between an assistant's tool calls and their results. Zero keeps
    /// no tail.
    pub tail_budget_tokens: usize,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            tail_budget_tokens: 2_000,
        }
    }
}

/// What one compaction did, for the host to display.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactReport {
    pub messages_before: usize,
    pub messages_after: usize,
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub est_tokens_before: usize,
    pub est_tokens_after: usize,
    /// Messages replaced by the summary (the head).
    pub head_messages: usize,
    /// Messages kept verbatim (the tail).
    pub tail_messages: usize,
    /// Stage 1: tool results elided into the store, and the bytes saved.
    pub elided_results: usize,
    pub elided_bytes: usize,
    /// Call ids whose results were elided (recoverable via the store).
    pub elided_call_ids: Vec<String>,
    pub summary: String,
    pub files_read: Vec<String>,
    pub files_modified: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CompactError {
    #[error("nothing to compact: the context fits within the tail budget")]
    NothingToCompact,
}

fn est_tokens(bytes: usize) -> usize {
    bytes / APPROX_BYTES_PER_TOKEN
}

pub(crate) fn estimated_context_tokens(context: &Context) -> u64 {
    est_tokens(context_bytes(context.messages())) as u64
}

fn message_bytes(message: &Message) -> usize {
    let bytes = serde_json::to_string(message).map(|s| s.len()).unwrap_or(0);
    let Message::Tool { results } = message else {
        return bytes;
    };
    results.iter().fold(bytes, |bytes, result| {
        let (count, data) = crate::tool_images::measure(&result.output);
        bytes.saturating_sub(data) + count * APPROX_IMAGE_TOKENS * APPROX_BYTES_PER_TOKEN
    })
}

fn context_bytes(messages: &[Message]) -> usize {
    messages.iter().map(message_bytes).sum()
}

/// Index where the verbatim tail starts. Walks backwards accumulating the
/// token estimate until the budget is spent, then moves forward to a safe
/// boundary: the tail must start at a user message or at an assistant
/// message (whose tool results, if any, follow inside the tail).
fn find_tail_cut(messages: &[Message], budget_tokens: usize, start: usize) -> usize {
    let mut acc = 0usize;
    let mut cut = messages.len();
    for (index, message) in messages.iter().enumerate().skip(start).rev() {
        acc += est_tokens(message_bytes(message));
        if acc > budget_tokens {
            break;
        }
        cut = index;
    }
    while cut < messages.len() && matches!(messages[cut], Message::Tool { .. }) {
        cut += 1;
    }
    cut
}

/// Stage 1: replace large head tool-result payloads with stubs, storing
/// the full original under the call id so `read_tool_result` can recover
/// it. Returns (elided call ids, bytes saved).
fn elide_tool_results(messages: &mut [Message], store: &TruncationStore) -> (Vec<String>, usize) {
    let mut elided = Vec::new();
    let mut saved = 0usize;
    for message in messages {
        let Message::Tool { results } = message else {
            continue;
        };
        for result in results {
            // The store keeps text; images are dropped for good.
            let (images, _) = crate::tool_images::measure(&result.output);
            let mut text = result.output.clone();
            crate::tool_images::take(&mut text);
            let Ok(full) = serde_json::to_string(&text) else {
                continue;
            };
            if full.len() < ELIDE_MIN_BYTES && images == 0 {
                continue;
            }
            let hint: String = full.chars().take(120).collect();
            store.insert(&result.call_id, &result.tool_name, full.clone());
            result.output = json!({
                "_elided": true,
                "hint": hint,
                "_readFull": format!(
                    "full output available: call read_tool_result with callId \"{}\"",
                    result.call_id
                ),
            });
            if images > 0 {
                result.output["_imagesDropped"] = json!(images);
            }
            elided.push(result.call_id.clone());
            saved += (full.len() + images * APPROX_IMAGE_TOKENS * APPROX_BYTES_PER_TOKEN)
                .saturating_sub(message_bytes_of_value(&result.output));
        }
    }
    (elided, saved)
}

fn message_bytes_of_value(value: &Value) -> usize {
    serde_json::to_string(value).map(|s| s.len()).unwrap_or(0)
}

/// Mechanical read/modified file ledger from the head's tool calls.
fn file_ledger(messages: &[Message]) -> (Vec<String>, Vec<String>) {
    let mut read = Vec::new();
    let mut modified = Vec::new();
    for message in messages {
        let Message::Assistant { tool_calls, .. } = message else {
            continue;
        };
        for call in tool_calls {
            let Some(path) = call.arguments.get("path").and_then(Value::as_str) else {
                continue;
            };
            let bucket = match call.name.as_str() {
                "read_file" => &mut read,
                "write_file" | "edit_file" => &mut modified,
                _ => continue,
            };
            if !bucket.iter().any(|p| p == path) {
                bucket.push(path.to_string());
            }
        }
    }
    (read, modified)
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}…")
}

/// One line per elided call: id, tool, and a compressed argument, so the
/// model can recover any dropped output by id. Without this index the
/// store keys would leave the model's memory with the head.
fn recovery_index(head: &[Message], elided_ids: &[String]) -> String {
    let mut lines = Vec::new();
    for message in head {
        let Message::Assistant { tool_calls, .. } = message else {
            continue;
        };
        for call in tool_calls {
            if !elided_ids.contains(&call.id) {
                continue;
            }
            let arg = call
                .arguments
                .get("path")
                .or_else(|| call.arguments.get("command"))
                .or_else(|| call.arguments.get("pattern"))
                .and_then(Value::as_str)
                .map(|a| truncate_chars(a, INDEX_ARG_CHARS))
                .unwrap_or_default();
            lines.push(format!("{} {} {}", call.id, call.name, arg));
            if lines.len() == INDEX_ENTRIES {
                let omitted = elided_ids.len().saturating_sub(INDEX_ENTRIES);
                if omitted > 0 {
                    lines.push(format!("(+{omitted} more in store)"));
                }
                return lines.join("\n");
            }
        }
    }
    lines.join("\n")
}

/// The deterministic summary: nothing here is paraphrased. Recent user
/// requests verbatim (capped), the recovery index, the last assistant
/// note. This text is the compaction floor.
fn deterministic_summary(head: &[Message], elided_ids: &[String]) -> String {
    let mut requests: Vec<&str> = Vec::new();
    let mut last_note = "";
    for message in head {
        match message {
            Message::User { content, .. } => requests.push(content),
            Message::Assistant {
                content: Some(text),
                ..
            } if !text.trim().is_empty() => last_note = text,
            _ => {}
        }
    }
    let requests: Vec<String> = requests
        .iter()
        .rev()
        .take(SUMMARY_REQUESTS)
        .rev()
        .map(|r| format!("- {}", truncate_chars(r, REQUEST_CHARS)))
        .collect();
    let index = recovery_index(head, elided_ids);
    let mut out = format!(
        "Prior context compacted ({} messages). Recover any output via \
         read_tool_result(callId).\n## Recent requests\n{}",
        head.len(),
        requests.join("\n"),
    );
    if !index.is_empty() {
        out.push_str(&format!("\n## Recovery index\n{index}"));
    }
    if !last_note.is_empty() {
        out.push_str(&format!(
            "\n## Last note\n{}",
            truncate_chars(last_note, NOTE_CHARS)
        ));
    }
    out
}

/// Compact `context` in place, deterministically: elide head tool
/// results into `store`, then rebuild the context as system prompt +
/// mechanical summary + verbatim tail. No model call is made.
pub fn compact(
    context: &mut Context,
    store: &TruncationStore,
    config: &CompactConfig,
) -> Result<CompactReport, CompactError> {
    let mut messages: Vec<Message> = context.messages().to_vec();
    let bytes_before = context_bytes(&messages);
    let messages_before = messages.len();

    let start = usize::from(matches!(messages.first(), Some(Message::System { .. })));
    let cut = find_tail_cut(&messages, config.tail_budget_tokens, start);
    // A head of one user message (or nothing) is not worth compacting.
    if cut <= start + 1 {
        return Err(CompactError::NothingToCompact);
    }

    let (elided_call_ids, elided_bytes) = elide_tool_results(&mut messages[start..cut], store);
    let head = &messages[start..cut];
    let (files_read, files_modified) = file_ledger(head);
    let summary = deterministic_summary(head, &elided_call_ids);

    let mut rebuilt = Context::new();
    if let Some(Message::System { content }) = messages.first() {
        rebuilt.push_system(content.clone());
    }
    rebuilt.push_user(summary.clone());
    for message in &messages[cut..] {
        rebuilt.push(message.clone());
    }

    let report = CompactReport {
        messages_before,
        messages_after: rebuilt.messages().len(),
        bytes_before,
        bytes_after: context_bytes(rebuilt.messages()),
        est_tokens_before: est_tokens(bytes_before),
        est_tokens_after: est_tokens(context_bytes(rebuilt.messages())),
        head_messages: cut - start,
        tail_messages: messages.len() - cut,
        elided_results: elided_call_ids.len(),
        elided_bytes,
        elided_call_ids,
        summary,
        files_read,
        files_modified,
    };
    *context = rebuilt;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_harness_core::testing::call;
    use orca_harness_core::ToolResult;

    fn result(id: &str, tool: &str, output: Value) -> ToolResult {
        ToolResult {
            call_id: id.into(),
            tool_name: tool.into(),
            output,
            is_error: false,
        }
    }

    /// A transcript: system, then two tool-using turns, then a plain
    /// exchange. Big tool outputs so eliding has something to save.
    fn transcript() -> Context {
        let big = "x".repeat(2_000);
        let mut context = Context::new();
        context.push_system("system prompt");
        context.push_user("read the config");
        context.push_assistant_tool_calls(
            None,
            vec![call("c1", "read_file", json!({"path": "Cargo.toml"}))],
        );
        context.append_tool_results(vec![result("c1", "read_file", json!({"content": big}))]);
        context.push_assistant_text("done reading");
        context.push_user("now edit it");
        context.push_assistant_tool_calls(
            None,
            vec![call("c2", "edit_file", json!({"path": "Cargo.toml"}))],
        );
        context.append_tool_results(vec![result("c2", "edit_file", json!({"ok": true}))]);
        context.push_assistant_text("edited");
        context
    }

    #[test]
    fn tail_cut_never_starts_at_a_tool_message() {
        let context = transcript();
        let messages = context.messages();
        for budget in [0, 100, 400, 1_000, 100_000] {
            let cut = find_tail_cut(messages, budget, 1);
            assert!(
                cut == messages.len() || !matches!(messages[cut], Message::Tool { .. }),
                "budget {budget} cut tail at a tool message"
            );
        }
    }

    #[test]
    fn eliding_stores_the_full_output_and_leaves_small_results_alone() {
        let store = TruncationStore::default();
        let mut messages = transcript().messages().to_vec();
        let (elided, saved) = elide_tool_results(&mut messages, &store);
        assert_eq!(elided, vec!["c1"], "only the large output is elided");
        assert!(saved > 1_000);
        assert!(store.get("c1").is_some(), "full original is recoverable");
        assert!(store.get("c2").is_none(), "small output was not stored");
        let Message::Tool { results } = &messages[3] else {
            panic!("expected tool message");
        };
        assert_eq!(results[0].output["_elided"], json!(true));
        assert!(results[0].output["_readFull"]
            .as_str()
            .unwrap()
            .contains("c1"));
    }

    #[test]
    fn images_count_at_a_flat_rate_and_are_dropped_when_elided() {
        let data = "A".repeat(400_000);
        let shot =
            json!({"content": "[image 1]", "_images": [{"media_type": "image/png", "data": data}]});
        let mut context = Context::new();
        context.append_tool_results(vec![result("c3", "shot", json!({"content": "[image 1]"}))]);
        let text_only = estimated_context_tokens(&context);
        context = Context::new();
        context.append_tool_results(vec![result("c3", "shot", shot)]);
        let overhead = estimated_context_tokens(&context) - text_only;
        // The key and image wrapper add a few tokens; the base64 adds none.
        assert!(
            (APPROX_IMAGE_TOKENS as u64..APPROX_IMAGE_TOKENS as u64 + 20).contains(&overhead),
            "image counted as {overhead} tokens"
        );

        let store = TruncationStore::default();
        let mut messages = context.messages().to_vec();
        let (elided, saved) = elide_tool_results(&mut messages, &store);
        assert_eq!(elided, vec!["c3"]);
        assert!(saved > APPROX_IMAGE_TOKENS * APPROX_BYTES_PER_TOKEN / 2);
        let Message::Tool { results } = &messages[0] else {
            panic!("expected tool message");
        };
        assert!(results[0].output.get("_images").is_none());
        assert_eq!(results[0].output["_imagesDropped"], json!(1));
        let (_, stored) = store.get("c3").unwrap();
        assert!(!stored.contains("AAAA"), "the store keeps text only");
    }

    #[test]
    fn compact_rebuilds_system_summary_tail_with_recovery_index() {
        let mut context = transcript();
        let store = TruncationStore::default();
        let config = CompactConfig {
            tail_budget_tokens: 60,
        };
        let report = compact(&mut context, &store, &config).expect("compacts");
        assert!(report.bytes_after < report.bytes_before);
        assert_eq!(report.files_read, vec!["Cargo.toml"]);
        assert_eq!(report.files_modified, vec!["Cargo.toml"]);
        let messages = context.messages();
        assert!(matches!(messages[0], Message::System { .. }));
        let Message::User { content, .. } = &messages[1] else {
            panic!("expected summary as a user message");
        };
        assert!(content.contains("read the config"), "request kept verbatim");
        assert!(
            content.contains("c1 read_file Cargo.toml"),
            "recovery index maps the elided call id to tool and path"
        );
        assert_eq!(
            messages.len(),
            2 + report.tail_messages,
            "system + summary + verbatim tail"
        );
    }

    #[test]
    fn summary_respects_hard_caps() {
        let mut context = Context::new();
        context.push_system("system");
        for i in 0..20 {
            context.push_user(format!("request {i} {}", "y".repeat(500)));
            context.push_assistant_text("z".repeat(5_000));
        }
        let store = TruncationStore::default();
        let config = CompactConfig {
            tail_budget_tokens: 0,
        };
        let report = compact(&mut context, &store, &config).expect("compacts");
        assert!(
            report.summary.len() < 1_200,
            "summary stays capped ({} chars)",
            report.summary.len()
        );
        assert_eq!(
            report.summary.matches("- request").count(),
            SUMMARY_REQUESTS,
            "only the most recent requests are kept"
        );
    }

    #[test]
    fn a_short_context_is_not_compacted() {
        let mut context = Context::new();
        context.push_system("system");
        context.push_user("hi");
        let store = TruncationStore::default();
        let err = compact(&mut context, &store, &CompactConfig::default());
        assert!(matches!(err, Err(CompactError::NothingToCompact)));
        assert_eq!(context.messages().len(), 2, "context untouched");
    }
}
