# Sandbox environments: a provider crate, two axes, enforced isolation

## Goal

Give the harness a first-class, provider-neutral sandbox boundary — the exact
analogue of what `crates/model-providers` did for models — so that an agent can
**run inside** a sandbox (enclosure) and/or **call out to** a sandbox as an
execution target, with both, either, or neither active, and so that a host which declares
isolation *enforced* cannot have that isolation dissolved by the agent.

Ship adapters for E2B, Daytona, Cloudflare Sandbox SDK, and Vercel Sandbox behind
one trait that stays open to future providers, plus a keyless local Docker adapter
so the crate is testable with no credentials.

## Approach

Add a `Sandbox` trait to `crates/harness-core` beside `Model`, a
`crates/sandbox-providers` crate holding the adapters and a shared REST core, and
a declared `Environment` on the session with two independent optional axes.
Enclosure **substitutes** the backends of the existing shell / process / REPL /
file tools; execution **adds** one `code_execution` tool. Reuse the existing `Executor`,
`Workspace`, `ToolPolicy`, `provider-auth`, and CLI `/provider` machinery rather
than building parallel structures. Where a required provider capability is absent,
fail closed at startup rather than degrade.

## Design constraints

- The kernel stays provider-neutral: `harness-core` gains a trait and types, never
  an HTTP client. Adapters live in the new crate, exactly as with `Model`.
- Every existing public signature of `Executor` (`local_sh`, `new`, `ssh`,
  `docker_exec`) is preserved byte-for-byte so no current call site churns.
- No second credential boundary. `orca-harness-provider-auth`'s `CredentialSource`
  / `BearerCredential` carry sandbox provider credentials too.
- No new heavyweight dependencies. `reqwest` is already in the workspace; Docker is
  driven through the `docker` CLI via the existing `Executor`, **not** `bollard`.
  The strict binary-size and startup budgets enforced in CI (commit `5bd2515d`) hold.
- Fixture-based wire tests per provider, mirroring `tests/openai_wire.rs` and
  `tests/openrouter_wire.rs`. No live API calls in the default test run.
- Enforcement is not advisory. A host-backed tool under `enforced` is a build-time
  error, not a warning and not a silent drop.
- `enforced` is immutable for the process lifetime. No slash command may set it.

## Vocabulary

The OpenAI Agents API already settled a sensible vocabulary for declared
environments, and users who have read those docs should find ours familiar. We
borrow the field names and keep our own provider tags.

Borrowed: `workspace_directory`, `capability_directories`, `packages
{python, npm, system}`, `setup_commands [{command, cwd}]`, `files`, `env`,
`network {access, allowed_domains}`, and `/workspace/outputs` as the
pull-back artifact directory.

Ours: the provider tags `e2b | daytona | cloudflare | vercel | docker`, and the
second (execution) axis, which has no counterpart there — both OpenAI environment
types are enclosure.

The second axis is named **execution**, and its tool is `code_execution` — the term
Anthropic uses for exactly this shape (a sandboxed bash-and-files environment the
model calls as a tool). Deliberately not "compute", which implies horsepower when the
real purpose is isolating untrusted model-generated code, often on a *smaller* machine
than the host; and not "remote", which fails to separate the axes, since an enclosure
is usually remote too and a local Docker enclosure is not remote at all.

Two details from that prior art are load-bearing here and are adopted deliberately:

1. Capability content (skills, plugins) is **staged into declared directories
   inside the sandbox**, not mirrored from the host. `skills/skill.rs:48` resolves
   skills from three host roots — workspace, config dir, home — and none of those
   exist inside a sandbox.
2. Their self-hosted executor serves shell, file read/write, **and local MCP** over
   one connection. MCP belonging inside the enclosure rather than on the host closes
   a hole that a shell-and-files-only design would leave open.

## Evidence and codebase overlay

| Existing location | Existing primitive / behavior | Reuse / proposed overlay |
| --- | --- | --- |
| `crates/harness-core/src/model.rs` | `Model` trait: kernel-side boundary, adapters in a separate crate | Structural template for `Sandbox`. New `sandbox.rs` beside it; kernel gains no HTTP. |
| `crates/harness-core/src/tool.rs` | Notes sandbox management belongs to Extensions, not Tools | Honored: provisioning/teardown is host-and-extension work; tools only consume an `Arc<dyn Sandbox>`. |
| `crates/model-providers/src/{http,http_error}.rs` | Shared HTTP core and error/retry classification across adapters | Same shape for the sandbox crate; one REST core, per-provider request/response shaping. |
| `crates/model-providers/src/catalog.rs` | Provider-neutral metadata exposed to hosts | Analogue: `capabilities.rs`, the provider-neutral capability report hosts and tool assembly read. |
| `crates/provider-auth/src/lib.rs` | `CredentialSource`, `BearerCredential`, `CredentialError` kinds | Used unchanged for sandbox API keys. `SandboxError` mirrors `CredentialError`'s kind/message shape. |
| `crates/tools/src/core_tools/shell.rs:27` | `Executor { program, leading_args }` → `tokio::process::Command` | Internals become an enum with a `Sandboxed(Arc<dyn Sandbox>)` arm. All public ctors unchanged; `Executor::sandbox(..)` added. |
| `crates/tools/src/core_tools/mod.rs:96` | `core_tools_with_executor` — documents that file tools still hit the **local** workspace | The hole enclosure must close. New `core_tools_with_environment` re-points file tools at the sandbox filesystem. |
| `crates/tools/src/core_tools/workspace.rs:2` | `Workspace` is a lexical path rooter; its own doc says it is *not* a sandbox and forward-references "the sandbox extension" | Gains a backend so `resolve` feeds either host FS or `Sandbox` file API. The forward reference is finally satisfied. |
| `crates/tools/src/bun_repl.rs`, `src/kernel.rs` | Persistent REPLs spawning local `tokio::process::Command` with framed stdin/stdout and pgroup kill | Refactored onto a `Spawner`; local impl is today's code verbatim, sandbox impl is `Sandbox::spawn`. This is what keeps PTC working inside an enclosure. |
| `crates/tools/src/core_tools/process.rs` | Long-lived sessions, stdin writes, background processes, pgroup cleanup | Same `Spawner` refactor. Requires the `sessions` capability; absent it, enclosure startup fails. |
| `docs/plan/2026-09-12-bun-programmatic-tool-calling.md` | PTC routes JS-issued tool calls through the real dispatcher; explicitly states "Native Bun is not a sandbox" | Enclosure is the answer to that caveat. PTC's duplex requirement is the reason `Sandbox::spawn` exists rather than `exec` alone. |
| `crates/extensions/src/policy.rs:34` | `ToolPolicy { allow, deny, rule }` | Runtime half of enforcement; catches tools registered after build (MCP reload). |
| `crates/tool-extensions/src/skills/skill.rs:48` | `roots(workspace, config_dir, home)` — three host roots | None exist in-sandbox. Skills are staged read-only into `capability_directories`. |
| `crates/cli/src/msg.rs:12` | `Provider` enum with `label` / `base_url` / `auth` / `key_env` / `ALL` | Template for `SandboxProvider`. Same shape, same `ProviderAuth` reuse. |
| `crates/cli/src/config/storage.rs` | Label-keyed `stored_key` / `save_key`; documented JSON shape at the top of the file | `save_key("e2b", …)` works today with no new code. Add a `"sandbox"` section and update the doc comment — it is that file's contract. |
| `crates/cli/src/tui/command_catalog.rs:269` | `/provider` catalog spec (and the catalog assertion at `:389`) | `/sandbox` is added the same way; the assertion is touched. |
| `crates/cli/src/tui/commands/dispatch.rs:378` | `/provider` opens `Overlay::Providers` with a `ListPicker` | `/sandbox` arm beside it, `Overlay::Sandbox`, plus render (`tui/render/shell/live.rs:218`) and keys (`tui/keys/overlays.rs:56`). |
| `crates/cli/src/runtime/mcp_reload.rs` | Rebuilds the agent mid-session after a config change | Precedent for an enclosure change mid-session, and the place the immutability rule is enforced. |
| `crates/cli/src/runtime/local_tools.rs` | Caches raw tool runtimes across MCP rebuilds | Sandbox-backed runtimes must not leak across an environment change; a rebuild replaces them. |
| `crates/cli/src/mode.rs`, `src/auto_approval.rs` | Yolo mode and automatic approval | Must be shown unable to widen the boundary: approval selects among registered tools, it does not register any. |

## Architecture

### The trait

```rust
// crates/harness-core/src/sandbox.rs
#[async_trait]
pub trait Sandbox: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    async fn exec(&self, req: ExecRequest) -> Result<ExecOutput, SandboxError>;
    async fn spawn(&self, req: SpawnRequest) -> Result<Box<dyn Session>, SandboxError>;
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError>;
    async fn write_file(&self, path: &str, bytes: &[u8], mode: FileMode) -> Result<(), SandboxError>;
    async fn list_dir(&self, path: &str) -> Result<Vec<Entry>, SandboxError>;
    async fn shutdown(&self) -> Result<(), SandboxError>;
}

/// A long-lived duplex process inside the sandbox: framed stdin in,
/// interleaved stdout/stderr out, killable. What `bun_repl`, `py_kernel`
/// and `process` require.
#[async_trait]
pub trait Session: Send + Sync {
    async fn write_stdin(&self, bytes: &[u8]) -> Result<(), SandboxError>;
    async fn kill(&self) -> Result<(), SandboxError>;
}
```

Output is **not** a method on `Session`. `spawn` returns
`(Box<dyn Session>, mpsc::Receiver<Chunk>)`, handing the receiver to the caller at
spawn time. This is deliberate and load-bearing: `bun_repl` and `kernel` already own
an `mpsc::Receiver` inside a `StdMutex`-guarded struct alongside `child` and
`stdin`, and receiving needs `&mut`. A `read_output(&self)` method would force
interior mutability and serialize every read against every other, and would not
compose with the locking shape those files already use. Passing the receiver
through lets the local implementation reuse its existing plumbing verbatim.

```rust

/// Separate from `Sandbox` so that a provider which owns the lifecycle
/// (declare-and-hand-over, as OpenAI's hosted environments do) is
/// implementable later without reshaping the consuming trait.
#[async_trait]
pub trait Provisioner: Send + Sync {
    async fn start(&self, spec: &EnvironmentSpec) -> Result<Arc<dyn Sandbox>, SandboxError>;
    async fn attach(&self, id: &str) -> Result<Arc<dyn Sandbox>, SandboxError>;
}
```

`Capabilities` reports `sessions`, `file_api`, `network_policy`, `artifacts`, and
`ports`. It is the provider-neutral fact that tool assembly reads, and the reason
enclosure can refuse to start coherently instead of half-working.

### The two axes

```rust
pub struct Environment {
    /// The agent's own shell/process/REPL/file tools run in here.
    pub enclosure: Option<Enclosure>,
    /// An additional `code_execution` tool the agent calls. Agent stays local.
    pub execution: Option<Execution>,
}
```

Independent `Option`s, so "both" and "neither" need no special cases. Enclosure is a
*substitution* of tool backends; execution is an *additive* `Tool`. They are different
code paths and must not be collapsed into one enum.

### Crate layout

```
crates/sandbox-providers/
  src/lib.rs
      http.rs  http_error.rs      shared REST core + retry classification
      spec.rs                     EnvironmentSpec, network policy, capability dirs
      capabilities.rs
      local/                      docker CLI through the existing Executor; keyless
      e2b/  daytona/  cloudflare/  vercel/
  tests/{e2b,daytona,cloudflare,vercel}_wire.rs
```

Each provider module follows `model-providers`' split: `mod.rs` (client and
lifecycle), `request.rs` (wire shaping), `stream.rs` where output streams.

### Workspace and capability materialization

- **Capability directories** (skills, plugins) are uploaded read-only into declared
  paths at start, re-staged when their source changes. Read-only protects the
  *integrity of host-supplied capabilities*: the agent cannot rewrite a skill the
  user installed and have the host's own instructions come back altered. It does not
  prevent the agent from authoring instructions of its own — `/workspace` is writable
  by design, and nothing stops an agent from writing a file there and acting on it.
  Do not claim otherwise.
- **User workspace** is uploaded at start honoring `.gitignore`. File tools operate
  inside. Changes return to the host on an explicit `/sandbox pull` or at session
  end, presented as a reviewable diff. Generated artifacts under
  `/workspace/outputs` are pulled the same way. No background sync daemon.

### Execution axis surface

One tool, `code_execution`, taking `{action: exec | read_file | write_file | upload |
download | reset, ...}`. One schema, one approval surface, and unambiguous to the
model that this is another machine rather than its own filesystem.

## Enforcement

`enforced: true` closes every host-touching surface, in two layers because either
alone has a gap: build-time refusal misses tools registered dynamically, and runtime
policy misses nothing but arrives after the tool already exists.

**Build time** — the builder *refuses to register*, with an error naming the tool:

- `fs_admin_tools` (host copy/rename/delete/mkdir/stat)
- any tool built on a local `Executor` or local `Spawner`
- host stdio MCP servers
- `SubagentTool` whose factory would construct host-backed child tools

**Runtime** — `ToolPolicy` denies those names, covering anything registered later
through `runtime/mcp_reload.rs`.

**Beyond the tool list:**

- MCP servers run *inside* the sandbox under enclosure; a host stdio server is
  refused when enforced.
- `enforced` is read once at startup from flag or config and is immutable for the
  process lifetime. `/sandbox` may display it and may never set it.
- The config dir and state dir live outside the sandbox workspace and are not
  writable by the agent — otherwise the agent rewrites `config.json` to disable
  enforcement and asks for a restart, which is the whole jailbreak in two steps.
- Yolo mode and auto-approval select among registered tools; they cannot register
  one, and a test asserts this.
- If `network.access: "restricted"` is required and the provider lacks the
  `network_policy` capability, startup fails.

## CLI

`/sandbox` clones the `/provider` chain end to end: catalog spec in
`command_catalog.rs` (and its `:389` assertion), `Overlay::Sandbox`, a dispatch arm
beside `dispatch.rs:378`, a render arm, and a key handler. The picker lists the
providers plus off; selecting one prompts for a key through the existing
label-keyed `save_key` / `stored_key`.

`config.json` gains:

```json
"sandbox": {
  "provider": "e2b",
  "mode": "enclosure | execution | both | off",
  "enforced": false,
  "image": "…",
  "workspace_directory": "/workspace",
  "network": { "access": "enabled", "allowed_domains": [] }
}
```

The JSON shape doc comment at the top of `storage.rs` is updated to match.
Flags: `--sandbox <provider>`, `--sandbox-mode <mode>`, `--sandbox-enforced`.
`enforced` is settable by flag or config only. A mid-session provider or mode change
rebuilds the agent through the `mcp_reload.rs` path and replaces cached runtimes in
`local_tools.rs`.

## Implementation steps and verification

- [x] **Land the trait and capability types, with no adapters.** Create
  `crates/harness-core/src/sandbox.rs` and export from `lib.rs`. Verify the crate
  still has no HTTP dependency, that `Sandbox` and `Session` are object-safe, and
  that a test double implementing both compiles and round-trips through
  `Arc<dyn Sandbox>`. No tool or CLI edits in this step.

- [x] **Refactor `Executor` internals with zero signature churn.** Modify
  `crates/tools/src/core_tools/shell.rs`. Verify every existing tools test passes
  untouched, that `local_sh` / `new` / `ssh` / `docker_exec` signatures are
  byte-identical (inspect the diff), and that a `Sandboxed` executor routes to a
  fake `Sandbox` rather than spawning a process. Add a test asserting no
  `tokio::process::Command` is constructed on that path.

- [ ] **Introduce `Spawner` and move the three stateful tools onto it.** Modify
  `crates/tools/src/bun_repl.rs`, `src/kernel.rs`, `src/core_tools/process.rs`; add
  `src/spawner.rs`. Settle the output-channel shape (above) before writing the local
  implementation — it is the likeliest place this refactor stalls, and it fails as a
  compile error rather than a behavioral difference. In scope beyond process
  spawning: `bun_repl`'s `TempSource`, which writes each call's source to
  `std::env::temp_dir()`, sets Unix permissions and cleans up on drop, then `.load`s
  that path *inside* the REPL — under enclosure that file must be written through
  `Sandbox::write_file` at a path the sandboxed `bun` can read. Confirm (do not
  assume) that `kernel.rs` needs no equivalent: its driver is passed via argv
  (`python3 -u -c '<driver>' <nonce>`), which has no host path. Verify the local
  `Spawner` is behaviorally identical to today —
  run the existing `bun_repl` and kernel suites unchanged, including timeout,
  cancellation, pgroup kill and restart-notice paths. Then verify the same suites
  against a fake in-memory sandbox `Session`. Bun must be installed for the real
  runtime tests; a skipped runtime test is not verification.

- [x] **Give `Workspace` a filesystem backend.** Modify
  `crates/tools/src/core_tools/workspace.rs` and the file tools. Verify path-escape
  rejection still holds on both backends, that `read/write/edit/patch/multi_edit/
  list` operate in-sandbox when so configured, and that `grep`/`glob` execute
  in-sandbox. Add the regression this closes: a sandboxed executor paired with file
  tools must not read a host file.

- [x] **Assemble by environment, failing closed.** Add
  `core_tools_with_environment` to `crates/tools/src/core_tools/mod.rs`. Verify that
  a provider reporting `sessions: false` makes enclosure startup fail with an error
  naming the provider and the tool, that the message is actionable, and that no
  partially-sandboxed tool set is ever returned.

- [x] **Build the sandbox-providers crate: shared core plus the keyless Docker
  adapter first.** Create `crates/sandbox-providers` with `http.rs`,
  `http_error.rs`, `spec.rs`, `capabilities.rs`, `local/`. Add to workspace members.
  Verify the Docker adapter satisfies the full trait including `spawn`, and run the
  tools suites from the previous steps against it as a real integration target. No
  `bollard`; shell out through `Executor`.

- [ ] **Add the four remote adapters, one at a time, fixture-tested.** Create
  `e2b/`, `daytona/`, `cloudflare/`, `vercel/` and their wire tests. Fetch each
  provider's current create/exec/files/session/kill contract from its live
  documentation before writing the adapter — not from recall. Verify request shaping
  and response parsing against recorded fixtures, error/retry classification through
  the shared `http_error`, and that each reports capabilities honestly (particularly
  `sessions`). No live calls in the default test run.

- [ ] **Capability staging and workspace transfer.** Add staging to the crate and
  wire skills/plugins through it. Verify capability directories land at the declared
  paths, are read-only to the agent (a write attempt fails), and are re-staged on
  change. Verify workspace upload honors `.gitignore`, that `/sandbox pull` produces
  a diff the user reviews before host files change, and that `/workspace/outputs`
  artifacts come back.

- [ ] **Enforcement, built to be attacked.** Add the builder refusals and the
  `ToolPolicy` deny set; create `crates/tools/tests/enforced_isolation.rs`. Verify
  one case per escape: `fs_admin_tools` refused; a local-`Executor` tool refused; a
  host stdio MCP server refused; a subagent with host-backed factories refused; a
  tool added post-build via the MCP reload path denied at runtime; yolo mode unable
  to widen the set; auto-approval unable to widen the set; the config file not
  writable from inside; `enforced` unchangeable at runtime; restricted network
  demanded of a provider that lacks it failing startup. Each is a named test.

- [ ] **The execution axis.** Add the `code_execution` tool. Verify each action, that
  it is registered only when `execution` is set, that it composes with an enclosure (both
  axes active simultaneously, pointing at different sandboxes), and that its
  approval is distinct from the agent's own tools.

- [ ] **SDK surface.** Modify `crates/sdk/src/{harness,agent,session,lib}.rs`.
  Verify `Harness::builder().environment(..)` and the per-agent execution option, that
  both axes are independently settable and independently absent, that a
  `pub mod sandbox` re-export mirrors `pub mod providers`, and that enforced
  configuration surfaces a build error rather than a runtime surprise.

- [ ] **CLI.** Modify `msg.rs`, `config/storage.rs` (including its doc comment),
  `command_catalog.rs`, `tui/commands/dispatch.rs`, `tui/render/shell/live.rs`,
  `tui/keys/overlays.rs`, `runtime/{mcp_reload,local_tools,startup}.rs`, and
  `main.rs` flags. Verify the picker, key capture and persistence, that the catalog
  assertion is updated deliberately, that a mid-session change rebuilds the agent
  and replaces cached runtimes, that `/sandbox` cannot set `enforced`, and that the
  active environment is visible in the status line.

- [ ] **Documentation and final review.** Create
  `docs/architecture/sandbox-environments.md` covering the two axes, the enforcement
  guarantees and their exact limits, the capability matrix per provider, and what
  `enforced` does *not* promise. Run the full validation set below. Inspect the
  final diff for: no HTTP in `harness-core`, no duplicated policy logic, no new
  heavyweight dependencies, no unrelated changes. Record the measured binary-size
  delta against the CI budget.

## Status at hand-over

Branch `feat/sandbox-environments`. Steps 1, 2, 4, 5 and the Docker half of step 6
are done: formatted, clippy-clean in the new code, and tested without any API key.

**Enclosure now works end to end for `shell` and every file tool.** An agent built
with `core_tools_in_sandbox` runs its commands and reads and writes its files inside
the provider, with nothing touching this machine.

Landed:

- `crates/harness-core/src/sandbox.rs` — `Sandbox`, `Session`, `Provisioner`,
  `Capabilities`, `Stat`, `SandboxError`. No HTTP in the kernel.
- `Executor` internals are `Process | Sandbox`, with every previous constructor
  unchanged. `ShellTool` routes to the provider when given one.
- `Workspace` gained a backend (`Workspace::sandboxed`) and an I/O facade — `read`,
  `read_opt`, `write`, `stat`, `list`, `remove_file`. All eight file tools
  (`read_file`, `write_file`, `edit_file`, `apply_patch`, `multi_edit`, `list_dir`,
  `grep`, `glob`) go through it instead of `tokio::fs`, so the boundary lives in one
  place rather than eight.
- `FileGuard`'s local `Stamp` collapsed into the kernel's `Stat`, so read-before-write
  means the same thing on both backends.
- `core_tools_in_sandbox(sandbox, workspace_dir)` — the only supported way to build a
  sandboxed set. Refuses a provider without `file_api` rather than returning one
  rooted on the host, and omits the three stateful tools that have no sandbox
  backend yet.
- `crates/sandbox-providers` — `EnvironmentSpec` (OpenAI's vocabulary), `Network`,
  `Packages`, and the keyless Docker adapter implementing the full trait.
- `orca_harness_sdk::sandbox` re-export module.

Tests, 15 new, all passing with no credentials:

- `tools/tests/sandbox_executor.rs` (4) — a sandboxed shell never reaches the host,
  cancellation wins before the provider is called, empty commands are refused, and
  the old `Executor` constructors still exist.
- `tools/tests/sandbox_workspace.rs` (6) — writes land in the provider and not on
  disk; read-before-write still refuses an unread overwrite *inside* a sandbox;
  `list_dir`/`grep`/`glob` walk the sandbox tree; path escapes are still refused; the
  assembled set contains no host-backed tool; a provider without a file API yields no
  set at all.
- `sandbox-providers` unit tests (5) and `tests/docker_roundtrip.rs` (1) — exec,
  non-zero exit, a binary-safe file round trip through a path containing a quote and
  a space, listing, `stat` presence and absence, and a live session driven over
  stdin. Ran against a real daemon; skips where none is available.

Two decisions made during implementation:

1. **No `From<SandboxError> for ToolError`.** It gave `?` two candidate conversions
   and broke inference in `tool-extensions/src/mcp/catalog.rs`. Callers convert
   explicitly.
2. **`Stat::modified` is a `SystemTime`, not whole seconds.** Truncating would let
   two writes in the same second with the same length compare equal — precisely the
   case read-before-write exists to catch. Providers reporting only seconds land on a
   second boundary; the host keeps full precision.

Not yet done, in order:

- **Step 3, the `Spawner` refactor** (`process`, `bun_repl`, `py_kernel`), including
  `bun_repl`'s `TempSource` host-path dependency. Until it lands, those three tools
  have no sandbox backend and `core_tools_in_sandbox` deliberately omits them, so a
  sandboxed agent has no persistent shell session and no REPL.
- Steps 7 onward: the four remote adapters, capability staging and workspace
  transfer, enforcement, the execution axis, and the CLI.

Known rough edge: under a sandbox, `grep` and `glob` walk the tree one directory per
round trip, which is fine for a shallow tree and slow for a deep one. The fix is to
push the walk into a single in-sandbox `find`/`rg` invocation; deferred rather than
hidden.

Pre-existing on `main`, not introduced here: four `clippy::result_large_err` errors
in `harness-core` under the CI gate (a newer clippy lint against `HarnessError` /
`ModelError`), and a failure in
`tool-extensions/tests/web.rs::concurrent_fetches_fan_out_through_the_dispatcher`
(a timing assertion). Both reproduce on a clean checkout.

## Validation commands

- `cargo fmt --all -- --check`
- `cargo test -p orca-harness-core`
- `cargo test -p orca-harness-tools`
- `cargo test -p orca-harness-sandbox-providers`
- SDK and CLI test packages by their exact manifest names
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- The CI binary-size and startup-budget checks introduced in `5bd2515d`

Run the Bun and Python runtime tests in an environment where both interpreters are
installed. Run the Docker adapter's integration tests where a Docker daemon is
available; remote adapters stay fixture-only by default.

## Review checkpoint

Steps 1–6 are the first independently valuable slice: the trait, the `Executor` and
`Spawner` refactors, the `Workspace` backend, environment-driven assembly, and one
working keyless Docker adapter. That slice alone closes the hole documented in
`core_tools_with_executor` and satisfies the forward reference in `workspace.rs`,
with no credentials and no remote provider involved. Review there before the four
remote adapters and the enforcement work land.

## Blast radius and explicit trade-offs

Model providers, the dispatcher, the agent loop, the DAG runtime, memory and
sessions are unchanged. The concentrated risk is in three places: the `Executor`
internals refactor (mitigated by signature preservation and running the existing
suites untouched), the `Spawner` extraction from three stateful tools (mitigated by
proving the local implementation behaviorally identical before any sandbox
implementation exists), and the `Workspace` backend (mitigated by keeping the
path-escape tests authoritative on both backends).

The deliberate costs: enclosure fails to start on providers without duplex
sessions rather than degrading, so some provider/tool combinations are simply
unavailable — chosen because a silently host-spawned REPL inside a "sandboxed"
agent is exactly the failure this work exists to prevent. Workspace changes require
an explicit pull rather than appearing live on the host. Adding a provider means a
new adapter module, not configuration.

What `enforced` does not promise: it constrains the agent's tool surface, not the
sandbox provider's own security boundary. Escaping the microVM or container is the
provider's threat model, not ours, and the documentation must say so plainly.

## Planning status

Read-only inspection of the repository completed. No implementation performed. No
sandbox provider API contract has been fetched yet — each adapter step begins by
reading that provider's current documentation, since wire formats in this space
move quickly and recall is not a source. The capability reports for the four remote
providers (in particular whether each supports long-lived duplex sessions) are
assumptions until verified against their documentation, and they determine which
provider/tool combinations are viable.
