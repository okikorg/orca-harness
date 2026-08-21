# Session Handler Extension

Date: 2026-08-21
Status: Approved

## Problem

The harness keeps the transcript only in memory. The CLI worker owns a
`Context` across turns; config persists provider, keys, and approvals, but
an exit or crash loses the conversation and nothing can be resumed.
`context.rs` already declares the boundary: "Context is model-visible
state, not durable memory. Durable memory is an Extension concern." This
spec fills that concern.

## Goal

Persist the session transcript to disk as the run progresses, and let a
session be reloaded and continued later (`orcacode --continue` /
`--resume <id>`). No kernel changes.

## Approaches considered

- **A. Extension-based recorder (chosen).** A `SessionHandler` extension
  in `crates/extensions` appends context messages to a JSONL file; a
  loader rebuilds a `Context` for resume. Works for CLI, headless, and
  library users alike.
- **B. EventStream sink.** Persist `HarnessEvent` NDJSON and replay it.
  Rejected: events are observability-shaped, not context-shaped;
  reconstruction is lossy around compaction and denied tools.
- **C. CLI-side snapshot.** Worker serializes `Context` after each turn.
  Rejected: CLI-only, and a mid-turn crash loses the whole turn.

## Design

### Component

New module `crates/extensions/src/session.rs` exporting:

- `SessionHandler` — the `Extension` that records the transcript.
- `SessionFile` — load/list helpers for resume and session pickers.
- `SessionMeta` — the header record (id, timestamps, workspace, model).

Re-exported from `orca_harness_extensions` alongside the existing
extensions. No changes to `harness-core`.

### File format

One JSONL file per session:

```
<sessions_dir>/<workspace-key>/<session-id>.jsonl
```

- `sessions_dir` defaults to the CLI config directory (the parent of
  `config_path()`) plus `sessions/`; library users pass any directory.
- `workspace-key` is a filesystem-safe encoding of the canonical
  workspace path (sanitized path plus a short hash to avoid collisions).
- `session-id` is a timestamp-prefixed unique id, so lexical order is
  chronological order.

Line 1 is the header: `{ "v": 1, "id", "created_at", "workspace",
"model" }`. Every subsequent line is one serialized `Message` (serde
derives already exist on `Message`, `ToolCall`, `ToolResult`).

Append-only JSONL means a crash loses at most the line being written,
and matches the NDJSON precedent in `events.rs`.

### Extension behavior

Subscriptions: `before_model`, `after_model`, `on_agent_end` only. The
handler keeps a cursor — the count of messages already persisted —
behind a `Mutex`.

On each hook, compare `context.messages().len()` to the cursor:

- `len > cursor`: append the new messages, flush, advance the cursor.
- `len < cursor`: the context was rewritten (`compact` or `/clear`);
  truncate the file and rewrite header plus all current messages, then
  set the cursor to `len`. Correct without knowing compaction internals.
- `len == cursor`: no-op.

Hook coverage: `before_model` catches the user prompt and prior tool
results (both are in the context before the model call); `after_model`
catches assistant turns; `on_agent_end` is the final flush and also
covers `Agent::run` paths that end without a further model call.

### Resume

- `SessionHandler::create(dir, meta)` starts a fresh file, writes the
  header, cursor 0.
- `SessionHandler::resume(path)` parses the header, replays lines into a
  `Context`, returns `(handler, meta, context)` with the cursor set to
  the loaded message count, and continues appending to the same file.
- `SessionFile::list(dir)` returns sessions newest-first (id order) with
  their headers, for `--resume <id>` matching and future pickers.

### CLI wiring

- `orcacode --continue` resumes the latest session for the current
  workspace; `--resume <id>` picks a specific one. Both work in
  interactive and headless (`--prompt`) mode; if no matching session
  exists, startup fails with an error.
- The worker seeds its `Context` from the loaded transcript instead of
  `Context::new()`.
- The handler is created once per session in `run_mode` and registered
  in `build_agent` via `extension_arc`, so it survives model/provider
  rebuilds the same way `TruncationStore` does.
- `/clear` starts a new session file; the old file is left intact.
- Every session, interactive and headless, records by default. A
  `--no-session` flag (and matching config field) opts out.

### Error handling

- Persistence failure must not kill the run. On a write error the
  handler emits one warning through a host-provided callback (the CLI
  routes it to a `UiMsg` notice) and disables itself for the rest of the
  run. Subsequent hooks are no-ops.
- Resume of a corrupt file fails loudly at startup, naming the file and
  the offending line number. One exception: a truncated final line (the
  crash artifact the format anticipates) is dropped with a warning and
  resume proceeds. A dropped final line can orphan a tool-call/result
  pair; the loader also drops a trailing `Assistant` message whose tool
  calls have no following `Tool` results, so the resumed context is
  always well-formed.
- A header whose `v` is unknown is an error (no silent migration).

### Out of scope (v1)

- Persisting the `TruncationStore`. After resume, `read_tool_result`
  for pre-resume entries returns its normal miss error; the store starts
  empty.
- Subagent transcripts. Only the root agent's context is recorded; the
  subagent tool does not register the handler.
- Retention/GC policy for old session files.

## Testing

Unit (extensions crate):

- Cursor append and advance across hooks.
- Rewrite-on-shrink after a simulated compaction and `/clear`.
- Header round-trip; unknown version rejected.
- Truncated final line dropped with warning; orphaned trailing
  tool-call message dropped.
- Write-error path: handler disables itself, callback fired once.

Integration:

- Record a scripted run via `testing.rs` fakes, resume it, assert the
  rebuilt `Context` equals the original messages.
- CLI-level: `--continue` seeds the worker context (behind the existing
  CLI test setup if present; otherwise covered by the loader test).
