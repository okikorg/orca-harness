//! Display-only history budgets. Execution and completion delivery are never capped here.

use super::{App, ToolActivity};
use std::collections::HashMap;
use std::io::Write;

/// Keep recent completed work discoverable without retaining an entire long session.
pub(crate) const COMPLETED_AGENT_HISTORY: usize = 100;
/// Each activity batch retains its newest settled calls, plus all outstanding calls.
pub(crate) const SETTLED_ACTIVITY_HISTORY: usize = 32;
pub(crate) const THINKING_HISTORY: usize = 32;
/// Tool previews and streamed display text are bounded independently of row counts.
pub(crate) const DISPLAY_TEXT_BYTES: usize = 8 * 1024;
const TRUNCATED: &str = " … [history truncated]";

pub(crate) fn bounded_text(text: &str) -> String {
    let mut result = String::new();
    append_display_text(&mut result, text);
    result
}

pub(crate) fn append_display_text(target: &mut String, text: &str) {
    let available = DISPLAY_TEXT_BYTES.saturating_sub(target.len());
    if text.len() <= available {
        target.push_str(text);
    } else if available > 0 {
        let mut end = available.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        target.push_str(&text[..end]);
        target.push_str(TRUNCATED);
    } else if target.len() == DISPLAY_TEXT_BYTES {
        // A later chunk can exceed an exactly full prior chunk. Mark it once.
        target.push_str(TRUNCATED);
    }
}

/// Serialize only up to the preview budget, avoiding a full extra allocation for huge results.
pub(crate) fn display_value(value: &serde_json::Value) -> serde_json::Value {
    struct Preview(Vec<u8>);
    impl Write for Preview {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let count = bytes
                .len()
                .min(DISPLAY_TEXT_BYTES.saturating_sub(self.0.len()));
            self.0.extend_from_slice(&bytes[..count]);
            if count == bytes.len() {
                Ok(count)
            } else {
                Err(std::io::Error::other("display budget reached"))
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut preview = Preview(Vec::new());
    if serde_json::to_writer(&mut preview, value).is_ok() {
        value.clone()
    } else {
        serde_json::Value::String(format!(
            "{}{TRUNCATED}",
            String::from_utf8_lossy(&preview.0)
        ))
    }
}

/// Evict settled rows only; remap every pending index after compaction. Outstanding
/// IDs necessarily scale with real concurrent work so late results cannot be lost.
pub(crate) fn compact_activity(
    tools: &mut Vec<ToolActivity>,
    pending: &mut HashMap<String, usize>,
) -> usize {
    let settled = tools.len().saturating_sub(pending.len());
    let mut discard = settled.saturating_sub(SETTLED_ACTIVITY_HISTORY);
    if discard == 0 {
        return 0;
    }
    let removed = discard;
    let mut index = 0;
    tools.retain(|tool| {
        let active = pending.contains_key(&tool.call_id);
        if !active && discard > 0 {
            discard -= 1;
            false
        } else {
            if active {
                pending.insert(tool.call_id.clone(), index);
            }
            index += 1;
            true
        }
    });
    removed
}

impl App {
    pub(crate) fn invalidate_agent_list(&self) {
        self.agent_list_cache.borrow_mut().take();
    }

    pub(crate) fn retain_agent_history(&mut self) {
        let mut groups: HashMap<u64, Vec<u64>> = HashMap::new();
        for transcript in self.subagent_transcripts.values() {
            let mut root = transcript.id;
            let mut remaining = self.subagent_transcripts.len();
            while remaining > 0 {
                let Some(parent) = self
                    .subagent_transcripts
                    .get(&root)
                    .and_then(|t| t.parent_id)
                else {
                    break;
                };
                if !self.subagent_transcripts.contains_key(&parent) {
                    break;
                }
                root = parent;
                remaining -= 1;
            }
            groups.entry(root).or_default().push(transcript.id);
        }
        let mut completed = Vec::new();
        let mut count = 0;
        for ids in groups.into_values() {
            if ids
                .iter()
                .any(|id| self.subagent_transcripts[id].status.is_active())
            {
                continue;
            }
            count += ids.len();
            let finished = ids
                .iter()
                .map(|id| {
                    let t = &self.subagent_transcripts[id];
                    t.started + t.elapsed.unwrap_or_default()
                })
                .max()
                .unwrap();
            completed.push((finished, ids));
        }
        if count <= COMPLETED_AGENT_HISTORY {
            return;
        }
        completed.sort_unstable_by_key(|(finished, _)| *finished);
        for (_, ids) in completed {
            if count <= COMPLETED_AGENT_HISTORY {
                break;
            }
            count -= ids.len();
            for id in ids {
                self.subagent_transcripts.remove(&id);
                self.evicted_agent_histories = self.evicted_agent_histories.saturating_add(1);
            }
        }
        // A plan outlives its stages' transcripts but not its own run row:
        // once the run is unreachable in the browser, so is its graph.
        self.workflows
            .retain(|run, _| self.subagent_transcripts.contains_key(run));
        let workflows = &self.workflows;
        self.workflow_stages
            .retain(|_, (run, _)| workflows.contains_key(run));
        self.invalidate_agent_list();
        if let Some(browser) = &mut self.agent_browser {
            browser.body_cache = None;
        }
    }
}

/// Aggregate serialized display payload budget; row counts alone allow huge batches.
pub(crate) const TRANSCRIPT_HISTORY_BYTES: usize = 256 * 1024;

pub(crate) fn tool_history_bytes(tool: &ToolActivity) -> usize {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    let _ = serde_json::to_writer(&mut counter, &tool.input);
    if let Some(output) = &tool.output {
        let _ = serde_json::to_writer(&mut counter, output);
    }
    counter.0
        + tool.call_id.len()
        + tool.call_line.len()
        + tool.tool_name.len()
        + tool.approval.as_ref().map_or(0, String::len)
        + std::mem::size_of::<ToolActivity>()
}

pub(crate) fn history_entry_bytes(entry: &super::SubagentTranscriptEntry) -> usize {
    use super::SubagentTranscriptEntry;
    std::mem::size_of::<SubagentTranscriptEntry>()
        + match entry {
            SubagentTranscriptEntry::Assistant(text) | SubagentTranscriptEntry::Error(text) => {
                text.len()
            }
            SubagentTranscriptEntry::Activity { thinking, tools } => {
                thinking.len() * std::mem::size_of::<super::ThinkingRecord>()
                    + tools.iter().map(tool_history_bytes).sum::<usize>()
            }
        }
}
