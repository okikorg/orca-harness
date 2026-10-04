# Sandbox provider API review: common shape and divergence

Reviewed before implementation, per the sandbox-environments plan. Sources are each
provider's current published API reference, not recall.

## The common shape

All four converge on the same five operations, which is what makes one trait viable:

1. **Create** a sandbox from an image/snapshot, with env, timeout, and (sometimes) a
   network policy — returns an id.
2. **Exec** a command with `cwd`, `env`, `timeout` — returns `stdout`, `stderr`,
   `exit_code`.
3. **Stream** a long-running command's output incrementally.
4. **Files**: write and read by absolute path.
5. **Destroy**.

Every provider also separates a *platform* plane (create/list/kill, authenticated
with an account API key) from a *sandbox* plane (exec/files, addressed per sandbox).
E2B makes this explicit with two hosts; Vercel with session ids in the path;
Cloudflare with a per-sandbox path segment. The adapter split follows that seam.

## Where they diverge, and why it matters

| | E2B | Daytona | Cloudflare | Vercel |
| --- | --- | --- | --- | --- |
| Transport | Connect RPC over HTTP, `application/connect+json` | plain REST | plain REST (bridge Worker) | plain REST |
| Exec | `POST /process.Process/Start` (server-streaming) | `POST /process/execute` | `POST /v1/sandbox/:id/exec` `{argv, cwd, timeout_ms}` | `POST /v2/sandboxes/sessions/:id/cmd` → `cmdId`, async |
| Output streaming | stream of `ProcessEvent` | `GET …/command/:id/logs`, WS for async | `streamLogs()` async iterator | `GET …/cmd/:id/logs`, ND-JSON |
| **stdin to a live process** | **yes** — `SendInput` / `StreamInput`, plus PTY | **yes** — `send_session_command_input(session, cmd, data)`, plus `createPty` + `sendInput` | not exposed | **no** |
| Files | Filesystem RPC | `upload_file` / `download_file` / `list_files` | `GET/PUT /v1/sandbox/:id/file/*` | `POST …/fs/write`, **gzipped tarball only** |
| Network policy | `network.rules` at create | — | — | `networkPolicy {mode, allowedDomains, allowedCIDRs}` |
| Kill | `kill()`, `setTimeout()` to extend | `delete` | — | `POST …/cmd/:id/kill {signal}` |

Three consequences for the implementation:

**1. Only E2B and Daytona can host an enclosure with the REPL tools.** `bun_repl`
and `py_kernel` need framed stdin to a live process. E2B (`SendInput`/`StreamInput`)
and Daytona (`send_session_command_input`, `createPty`) both provide it. Vercel's
command API is fire-and-forget with log streaming and a kill signal — no stdin path
at all. Cloudflare's `startProcess` streams output and kills, but exposes no input
channel.

So Vercel and Cloudflare report `sessions: false`, and enclosure with the default
coding preset fails closed on them, naming the capability and the tools that needed
it. They remain fully usable for the execution axis and for enclosure with a
shell-and-files tool set. This is the fail-closed decision earning its keep on the
very first four adapters, rather than a hypothetical.

**2. Cloudflare is not directly reachable from a non-JS client.** The SDK is a
Durable Object binding used *inside* a Worker (`getSandbox(env.Sandbox, id)`); the
container speaks HTTP only behind that DO. The repo does ship `bridge/worker`, a
REST facade (`POST /v1/sandbox/:id/exec`, `GET /v1/sandbox/:id/file/*`, bearer
`SANDBOX_API_KEY`), and that is what a Rust adapter targets. The adapter therefore
requires the user to deploy the bridge and configure its URL — documented as a
precondition, not a silent failure.

**3. Two wire quirks that do not generalize.** E2B uses Connect protocol framing
with mandatory `Connect-Protocol-Version: 1` plus `E2b-Sandbox-Id` /
`E2b-Sandbox-Port` routing headers — JSON over HTTP, so no gRPC dependency is
needed, but the envelope is not plain REST. Vercel accepts file writes *only* as a
gzipped tarball with `Content-Type: application/gzip`, extracted at `x-cwd`; a
single-file write means building a one-entry tar. Both stay inside their own adapter
module; neither leaks into the shared core.

## What this implies for the shared core

Justified as shared (`http.rs`, `http_error.rs`): bearer auth, JSON request/response
plumbing, status-to-error-kind classification, retry/backoff on 429 and 5xx, and
timeout handling. Every provider needs exactly these.

Deliberately *not* shared: streaming decode (three incompatible framings) and file
transfer encoding (raw bytes vs multipart vs tarball). Forcing a common abstraction
over those would be indirection without reuse — each adapter implements the trait
method directly.

`Capabilities` is therefore a real runtime value, not decoration: `sessions` is false
for two of the four providers reviewed here, and that fact decides which tool sets
can be assembled.
