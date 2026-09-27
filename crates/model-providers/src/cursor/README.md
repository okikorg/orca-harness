# Cursor native Agent adapter

Native `agent.v1.AgentService` integration, not an OpenAI proxy.

## Integration

Exported as `CursorModel` by the provider crate and SDK. Its native transport
uses HTTP/2, protobuf, SHA-256-addressed blobs, and UUID continuation IDs.

Public API: `CursorModel::new(model).base_url(url).api_key(access_token)`;
`list_models(base_url, key).await`. Implements core generate and
 generate_streaming with the same incremental transport.
There is deliberately no max_tokens builder: the inspected native protocol
provides no evidenced output-token-limit parameter. No silent no-op setting.

## Behavior

- HTTP/2 streaming request body, five-byte Connect envelopes, protobuf fields,
  periodic client heartbeats, SHA-256-addressed history and KV get/set replies.
- Current user text/images, system rules and text/tool history, MCP schemas as
  google.protobuf.Value, typed MCP arguments, text and reasoning deltas.
- Live connection retained at a tool request; the next core tool result resumes
  that connection using preserved native exec identifiers. Local unique core
  call IDs route independent continuations without a global registry.
- Tool output text and `_images` use native MCP text/image result blocks.
- Output token deltas accumulate; other usage fields stay zero/None because
  this protocol subset does not report them. No estimated usage/pricing.
- Unary protobuf model discovery returns identifiers and display names.
- Explicit malformed framing/protobuf, compressed-frame, Connect error,
  authentication, idle-timeout and premature-EOF failures.

## Limitations

Private protocol and pinned client version may change. No authenticated live
Cursor account test was performed. Tests validate local wire fixtures and
continuation behavior, not service acceptance or HTTP/2 network integration.
Historical user-image replay is explicitly rejected rather than silently
lost. Historical tool-image replay is represented in JSON, not image blocks;
use live continuations for image tool results. No native Cursor tool execution,
interactive queries, compressed frames, OAuth refresh, persistent checkpoints,
reasoning controls, max mode, or cross-process continuation restoration.
Only one MCP call is returned per core step; subsequent calls remain buffered.
Keep the same model (or clone) across steps and do not alter tools/credentials
mid-continuation. Missing/expired Cursor continuations fail rather than restart.

Sessions expire after one hour and response reads have a 120-second idle
limit. At most 32 paused sessions are retained, with expired entries evicted
on the next tool pause; dropping the model drops all retained connections.
Paused responses are not actively drained during tool execution. Heartbeats
continue during that pause for up to one hour. KV storage is capped at 64 MiB
and individual frames at 32 MiB. There is no automatic retry of native runs.

## Validation

Nine tests passed via an external temporary Cargo harness importing the actual
adapter, core, catalog, HTTP-error and tool-image sources. This avoids changing
workspace lib/Cargo files. Tests cover framing splits/coalescing, invalid wire,
headers/auth/Connect errors, request history/blob/image/schema encoding, typed
JSON values, MCP calls/results/continuations, KV replies, usage and EOF errors.
