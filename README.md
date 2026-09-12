# Orca Harness

A minimal, high-performance agent execution kernel in Rust. It keeps the
privileged path tiny and adds capabilities through Extensions.

> **The loop is sacred. Everything around it is extensible.**

Orca Harness is an execution primitive, not the whole Orca product. It is a
standalone Cargo workspace inside the monorepo: nothing in the Go control
plane or the Node sidecars depends on it, and it depends on nothing here.

For a visual walkthrough of the design — the loop, dispatcher, extensions,
tools, lifecycle, performance, and boundary — see
[`docs/design.html`](docs/design.html).

## Boundary

| Owner                      | Responsibilities                                                                                                                                                                                         |
| :------------------------- | :------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **The Harness owns**       | agent configuration, context, model invocation, the loop, tool dispatch, concurrent execution, cancellation, deadlines, limits, call/result pairing, deterministic ordering, and the Extension lifecycle |
| **The surrounding system** | distributed scheduling, durable sessions, microVM lifecycle, networking, tenancy, fleet management, and control-plane APIs                                                                                |

```text
         AROUND THE HARNESS
┌────────────────────────────────────┐
│  scheduling  sessions  isolation   │
│  persistence  networking  scaling  │
└─────────────────┬──────────────────┘
                  ▼
┌────────────────────────────────────┐
│            ORCA HARNESS            │
│  Agent → Loop → Dispatcher → Tool  │
│                 ↕                  │
│             Extensions             │
└────────────────────────────────────┘
```

## Layout

```text
orca-harness/
├── Cargo.toml                 # workspace manifest
├── crates/
│   ├── harness-core/          # agent loop, model/tool contracts, dispatcher, limits, testing
│   │   ├── src/
│   │   ├── examples/
│   │   ├── benches/
│   │   └── tests/
│   ├── provider-auth/         # provider-neutral credential contracts
│   │   └── src/
│   ├── model-providers/       # unified OpenAI, OpenRouter, and Codex adapters
│   │   ├── src/{openai,openai_codex,openrouter}/
│   │   ├── examples/
│   │   └── tests/
│   ├── tools/                 # shell/process, files/search, Python/Bun compute, subagents, todo, and ask
│   │   ├── src/
│   │   ├── examples/
│   │   └── tests/
│   ├── tool-extensions/       # opt-in MCP, skills, and web integrations
│   │   ├── src/{mcp,skills,web}/
│   │   ├── examples/
│   │   ├── benches/
│   │   └── tests/
│   ├── extensions/            # events, policy, memory, compaction, truncation, retry, usage, sessions
│   │   ├── src/
│   │   ├── examples/
│   │   ├── benches/
│   │   └── tests/
│   └── cli/                   # `orcacode` terminal host
│       └── src/
│           ├── auth/          # device login and credential storage
│           ├── config/        # persisted host configuration
│           ├── prompt/        # prompt parsing and file mentions
│           ├── runtime/       # agent construction, workers, sessions, and signals
│           ├── tui/           # commands, components, events, rendering, keys, and tests
│           ├── view/          # Markdown and transcript formatting
│           └── presentation/  # tool-result presentation
├── benchmarks/                # kernel/startup probes, reports, budgets, and results
├── ci/                        # test/benchmark workflows and size checks
└── docs/                      # crate diagram, design notes, and implementation plans
```

See [`docs/crate-diagram.md`](docs/crate-diagram.md) for the runtime dependency graph between these crates.

## Try it

`orcacode` is a reference host proving the harness drives a real terminal
agent. It talks to any OpenAI-compatible endpoint; with no key set it
defaults to a local Ollama server:

```bash
cargo run --release -p orcacode                    # interactive REPL
cargo run --release -p orcacode -- -p "..."        # headless: single prompt
cargo run --release -p orcacode -- --json -p "..." # headless: NDJSON events
cargo run --release -p orcacode -- --plan -p "..." # read-only: plan, change nothing
```

To use an OpenAI ChatGPT subscription instead of API billing, run Orcacode,
open `/provider`, and select `openai-codex`. Orcacode displays OpenAI's device
login URL and code, stores the resulting credentials under its own config
directory, and refreshes them automatically. An existing official Codex login
from `$CODEX_HOME/auth.json` or `~/.codex/auth.json` is imported as a fallback.
The `openai` provider remains the separate API-billed option.

### Interactive mode

Each user turn gets a strong transcript spine; live reasoning and parallel
tool calls group into a per-turn activity rail. Completed work collapses to
a compact summary:

- `ctrl+o` expands a turn's full output tree in place; `/expand <id>` prints
  one tool call's raw output.
- Tool rows use `□`, `✓`, and `×` for running, successful, and failed;
  failed output expands inline while work is live.
- Subagents render their inner tool calls as an indented nested rail,
  collapsed into the expandable record when they finish. The status line
  counts live background work (`procs 2 · pykernel · bun_repl · agents 3`).

**Scrolling and copying text out** — mouse capture is on, so the wheel
scrolls the transcript. `shift+↑`/`shift+↓` scroll by line and
`pgup`/`pgdn` by page, which matters on laptop keyboards where those are
`fn`+arrows that some terminals swallow for their own scrollback. Capture
does not cost you selection: hold `option` (macOS terminals) or `shift`
(most others) while dragging and the terminal draws its own selection as
usual. A terminal selection is always whole rows, though, so in split
view a drag takes both panes at once — there is no escape sequence that
would stop it at the divider. For one pane on its own, and for text that
has already scrolled past, the clipboard reaches further than a drag can:

| Key      | Command            | Effect                                                                    |
| :------- | :----------------- | :------------------------------------------------------------------------ |
| `ctrl+y` | `/copy`            | copy the last answer (`code` for its last fenced block, `all` for the whole transcript) |
| —        | `/copy tool`       | copy the tool currently shown in the split inspector |

`/copy` goes through OSC 52, so it reaches the clipboard of whichever
terminal is in front of you — including across `ssh`. Under `tmux` it
needs `set -g set-clipboard on`.
Copying mid-stream takes the partial answer as it stands, and says so.
`up`/`down` recall prompt history.

**Tool approvals** — gated tools (`shell`, `write_file`, `edit_file`,
`apply_patch`, `multi_edit`, `pykernel`, `bun_repl`, `subagent`) pause behind a prompt:

| Key | Effect                                                                           |
| :-- | :------------------------------------------------------------------------------- |
| `y` | allow once                                                                       |
| `a` | always allow for this session                                                    |
| `A` | always allow **and** save the grant to the config file, scoped to this workspace |
| `n` | deny                                                                             |

Saved grants are scoped to the workspace directory — future sessions in the same directory
skip the prompt, other directories still ask — and are listed and revocable from `/settings`.

**Plan mode** — `/mode plan` (or `--plan` at startup) makes the session
read-only: the agent investigates and proposes, but changes nothing.
Bare `/mode` opens a picker over all three modes, preselected on the
current one; `/mode plan`, `/mode yolo`, and `/mode normal` set a mode
directly. The status line carries `· plan mode` for as long as plan
mode is on.

It is an **allowlist**, not a denylist. Only `read_file`, `list_dir`,
`grep`, `glob`, `file_info`, `read_tool_result`, `memory_search`, `web_fetch`,
`web_search`, `web_crawl`, `skill`, `todo_write`, and `ask` run; everything else
— `shell`, `process`, `pykernel`, `bun_repl`, `write_file`, `edit_file`,
`apply_patch`, `multi_edit`, `subagent`, `memory_manage`, and every MCP tool — is denied
with a reason that points the model at the plan directory instead. A denylist would have to know every tool the
session might load, and MCP servers and skills add tools the CLI has
never heard of, so unknown means denied.

**The plan is a file, not a paragraph.** `docs/plan/` is the one writable
directory in plan mode: `write_file`, `edit_file`, `multi_edit`, and non-deleting
`apply_patch` calls pass the gate only when every target is a markdown file directly
inside it; every other write is refused.

**The agent decides whether to write a plan, and what to call it.** The
host has no way to tell a feature request from a greeting at the moment a
turn begins — deriving a filename from the first thing typed produces
`docs/plan/2026-08-22-hi.md`. So the host holds the fence and nothing
else. On the first plan-mode turn it pushes a system message with the
rules, the directory, today's date (which the model has no other way to
know), and the convention `docs/plan/YYYY-MM-DD-<feature-name>.md` — then
leaves the judgment alone. A question gets an answer; work that spans
several steps gets a file.

Writing goes through the ordinary `write_file` approval prompt, so you
are asked before anything lands on disk. Because the gate runs before
approval, an "always allow `write_file`" grant cannot widen past this
directory.

```text
• plan mode · read-only: the agent investigates and proposes, but changes nothing
  …investigation…
  write_file docs/plan/2026-08-22-rewind-command.md   [y/a/A/n]
• normal mode · every tool is available; gated tools ask for approval
• plan saved to docs/plan/2026-08-22-rewind-command.md
```

An episode ends when you return to normal mode, and `/mode normal` lists
the plans that were actually written — observed from tool results, not
guessed from the filesystem, so a denied or failed write is never
reported as saved. It says nothing when no plan was written: looking
around in plan mode is a legitimate use of it.

**Yolo mode** — `/mode yolo` (or `--yolo` at startup, or pick it from
the `/mode` picker) goes the other way: every gated tool runs without
approval prompts, interactive or headless (it implies `--auto-approve`
headless). Saved grants, revocations, and the approval UI are all
skipped while it is on — the mode outranks everything below it.

A mode that silences exactly the mechanism whose job is to say "wait"
must never be quiet itself, so while yolo is on:

- the status line carries `· yolo` for the whole session — not among
  the optional segments that come and go;
- it is never persisted: a session that silently came back with
  approvals off would be a trap rather than a convenience.

```text
• yolo mode · every tool runs without approval prompts
 model · running · yolo · ctx 10% · enter queue · esc interrupt · repo
```

Plan mode outranks yolo if both flags are given (`--yolo --plan` starts
in plan mode): read-only and unprompted is a coherent, safe stance for
unattended investigation, while letting the louder flag win would turn
an ambiguous invocation into "everything writable, nobody asked".

Note that this repository's own plans live in `docs/superpowers/plans/`,
the convention the `superpowers` plugin uses. `orcacode` writes to
`docs/plan/` — one constant, `plan::PLAN_DIR`, if you would rather it
matched.

`shell` is excluded on purpose: most of what an agent wants it for while
planning (`git log`, `cargo check`) is read-only, but deciding that from
a command string is guesswork, and a safety mode that guesses is not one.
The gate reads the mode on every tool call rather than at agent build, so
flipping it mid-run applies to every call the kernel has not yet checked,
not to the next run. A mode that took effect one turn late would be a
safety feature that lies. It is mirrored into spawned subagents the same
way tool retry is, so a subagent that was already running when the mode
flipped stops acting too — a restriction that only held at depth 0 would
not be one.

**Project instructions** — standing guidance is read at startup from
`AGENTS.md`, and all three locations apply:

| File                          | Scope                           |
| :---------------------------- | :------------------------------ |
| `$ORCA_CONFIG_DIR/AGENTS.md`  | every workspace on this machine |
| `<workspace>/AGENTS.md`       | this project                    |
| `<workspace>/.orca/AGENTS.md` | this checkout, unshared         |

`AGENTS.md` is the cross-agent convention, so a repository that already
has one works with no second file. Each is capped at 32 kB and appended
to the system prompt under a header naming its source, with the
precedence stated to the model: instructions beat the base prompt, and a
live request beats the instructions. Loaded files are reported in the
transcript (`instructions · AGENTS.md (1.2 kB)`).

Files are read once, at startup. `/clear` re-pushes the same composed
prompt, so instructions survive a reset — but an **edited** `AGENTS.md`
reaches the model on the next `orcacode`, and a **resumed** session
(`--continue`, `--resume`, `/sessions`) keeps the system prompt recorded
in its transcript, so changes reach it only after `/clear`.

**Task list** — `todo_write` gives the agent its plan as structured state
instead of prose it re-derives every turn. It takes the whole list on
every call and replaces what it held, so completing an item, adding a
step it discovered, and dropping one are all the same operation; exactly
one item may be `in_progress`. The status line shows progress
(`· todo 2/5`) and `/todo` prints the list:

```text
  todo · 1/3 done
  ✓ read the failing test
  ▸ fix the off-by-one
  □ run the suite
```

**Queuing and cancellation** — prompts submitted during a run wait in a FIFO
rail above the activity indicator and start automatically in submission
order (`/queue clear` discards them). Esc cancels the in-flight run (killing
spawned subprocesses) and pauses the queue; the conversation persists across
turns.

**Sessions** — recorded per workspace under `~/.config/orcacode/sessions/`
as append-only JSONL:

| Command / flag  | Effect                                                                                                     |
| :-------------- | :--------------------------------------------------------------------------------------------------------- |
| `--continue`    | resume the latest session                                                                                  |
| `--resume <id>` | resume a specific session                                                                                  |
| `/sessions`     | open a picker and resume from the TUI                                                                      |
| `--no-session`  | opt out of recording                                                                                       |
| `/clear`        | preserve the current transcript, start a fresh session, and stop all background work (processes, pykernel, bun_repl, subagents) |
| `/rewind [n]`   | drop the last `n` user turns (default 1) from the conversation and the file                                |
| `/fork`         | continue this conversation in a new session file, leaving the current one as it is                         |

`/rewind` always cuts on a user-turn boundary, so what remains can never
end in tool calls with no results — the shape a chat-completions endpoint
rejects. The context shrinks, so the session file is rewritten rather
than appended to, and the transcript is redrawn from what survives. The
session's token totals are not reset: those tokens were spent, and
rewinding does not un-spend them. The read-before-write guard *is*
cleared, because a dropped turn may have held the read that made a file
overwritable.

`/fork` is how a conversation branches. The current file is left exactly
where it was and recording moves to a new one carrying the whole context
so far, with the old session recorded as its `parent`. Rewinding and then
forking keeps the original line intact while the new one goes somewhere
else.

**Memory** — deliberately stored durable memory lives in one owner-only embedded SQLite
FTS5 database at `~/.config/orcacode/memory.sqlite3` (`$ORCA_CONFIG_DIR`
overrides the directory). There is no daemon, subprocess, Markdown mirror, or
background extraction. `memory_search` reads global plus current-workspace
records and is allowed in plan mode. `memory_manage` saves, updates, or forgets
records and follows the normal write approval gate; every save requires an
`is_global` boolean, while OrcaCode supplies the trusted workspace identity.
Tools expose only opaque 128-bit `mem_` identifiers; integer rowids remain
private to SQLite and its FTS joins.
The model is told that recall is automatic and may propose a write either when
the user asks or when directly stated, stable information is likely to help in
future sessions. It must avoid ordinary conversation, one-off task state,
duplicates, secrets, and unverified inferences. Workspace scope is the default;
clearly cross-workspace information may be proposed as global, and the normal
approval gate remains the user's final decision.
Before each model turn, `MemoryModel` uses `MemoryExtension` to retrieve a
bounded matching set and adds it at user authority to a transient request
clone, never to durable context or as policy or permission.
`cargo bench -p orca-harness-extensions --bench memory` compares direct SQL,
the scoped store API, and complete context hydration over 50,000 records.

**Endpoint selection** — `ORCA_MODEL`, `ORCA_BASE_URL`, and
`OPENAI_API_KEY` (or `--model`, `--base-url`, `--api-key`) choose the
provider. Keys entered in the TUI, the active provider, the theme, and the
last model picked per provider persist to
`~/.config/orcacode/config.json` (owner-only permissions; `$ORCA_CONFIG_DIR`
overrides the directory) and are reused on later runs — flags and
environment variables always win over saved values. `/settings` shows the
current values and jumps into the provider, model, theme, and api-key
pickers.

**Extensions** — `/extensions` opens a picker over the optional harness
extensions — output truncation (default on) and tool retry (default off);
enter toggles the selected one, and `/extensions enable\|disable <name>`
works directly. Toggles persist to the same config file and apply to
interactive and headless runs alike.

**MCP servers** — `/mcp` opens a picker over the configured stdio MCP
servers; space (or enter) toggles one on or off, and the row shows its
live tool count:

| Command                        | Effect                                            |
| :----------------------------- | :------------------------------------------------ |
| `/mcp`                         | picker — space toggles the selected server        |
| `/mcp add <name> <command>`    | save a server and connect it                      |
| `/mcp remove <name>`           | forget a server and drop its tools                |

MCP contributes three stable model-facing interfaces regardless of how many
servers or remote tools are connected:

- `mcp_search_tools` searches metadata across every server without exposing
  every full input schema.
- `mcp_select_tool` loads one exact search result; its ordinary
  `mcp__<server>__<tool>` schema appears on the next model turn.
- `mcp_features` lists/reads resources, lists/invokes prompts, and performs
  prompt/resource argument completion against an exact server.

Remote tools remain ordinary harness tools for dispatch, approval,
cancellation, and keyed per-server concurrency; a host-side model adapter
filters their schemas until selected, and execution rejects guessed tool names
until that same selection occurs. Connections validate initialization and
advertised capabilities, load every paginated tool-list page, and fail closed
on malformed protocol traffic or generated-name collisions. An interrupted
request closes its serialized connection rather than risking a stale or
partially written exchange; toggle the server to reconnect it. The harness core
and agent loop are unchanged. Server `<name>` must be letters, digits, `-`, or
`_`, and must not start with `-`. The command must begin with a stdio executable,
not an option such as `--url` or a bare HTTP URL. Invalid registrations are
rejected before saving; invalid existing entries remain visible in `/mcp` but
are not launched. Toggling is cheap: reloads diff the config against the live connections,
so flipping one server leaves the others' processes untouched. A server that
fails to connect reports why and is skipped.

Interactive startup and reload connect in the background, with up to four
connections in flight and a 30-second total connection timeout per server.
The welcome screen remains usable and shows `MCP connecting · tools pending`.
Catalog publication remains deterministic; slow connections do not block the
UI or ordinary tools. Once initialization completes, integration tools are
registered at the next worker command boundary, without changing a running
agent's tool set. Changes made during initialization queue one fresh reload.
Exiting cancels pending connection work.

Servers persist to the config file, either as a bare command string or as
`{"command": …, "enabled": false}` so a disabled server keeps its command:

```json
"mcp": {
  "fetch": "uvx mcp-server-fetch",
  "docs": { "command": "npx -y mcp-remote https://…", "enabled": false }
}
```

The command is split on whitespace and run without a shell — there is no
quoting, no variable expansion, and no per-server environment, so a server
needing a credential reads it from the environment the CLI was launched
with. The picker redacts credentials it can recognize (header values,
`--api-key`/`--token` arguments, URL userinfo and query secrets, bare
token-shaped words) so a pasted key is not left on screen; `${VAR}`
references are shown as written, since they name a variable rather than
carry one.

Remote HTTP/SSE servers are reachable through a stdio bridge:
`/mcp add docs npx -y mcp-remote https://mcp.example.com/mcp`.

**Agent Plugins** — Orcacode is an Agent Plugins 1.0 client for portable
Agent Skills and MCP-over-stdio packages. It also implements optional lifecycle
hooks as an Orcacode-specific client extension; hooks are not a portable Agent
Plugins 1.0 component. Scaffold either supported project, install its
dependencies yourself, then validate and exercise it before registration:

```sh
orcacode plugin init rl-python --py
orcacode plugin init rl-typescript --ts
orcacode plugin validate ./rl-python
orcacode plugin test ./rl-python
orcacode plugin install ./rl-python
orcacode plugin enable rl-python
```

Installation links the canonical local directory rather than copying it and
starts disabled; enable, disable, and uninstall changes apply to the next
Orcacode process. Orcacode does not directly run a dependency installer;
generated Python plugins may let their `uv` child resolve into `PLUGIN_DATA`
on first execution. Uninstall keeps both source and
`$ORCA_CONFIG_DIR/plugin-data/<name>/`. Plugin children receive
a sanitized environment, but they are ordinary user processes, not a sandbox:
keep secrets out of visible manifests and review code before enabling it.
`/plugin` opens the standard filterable picker. Its saved and live columns
distinguish next-launch enablement from tools loaded in the current TUI;
enter toggles, while space reveals inspect, validate, test, and uninstall
actions. Every CLI subcommand is also available as a typed `/plugin ...`
command. `/mcp` manages standalone MCP entries and also shows enabled plugin
servers as read-only rows; use `/plugin` for plugin changes. See the
[Agent Plugins guide](docs/external/index.html#plugins) for package layout,
TUI usage, runtime boundaries, and long-running tool design.

Each scaffold includes a tracked, empty `skills/` directory. Add a standard
`skills/<name>/SKILL.md`, then validate the package. Enabled plugin skills join
the ordinary `/skills` catalog and `skill` tool on the next launch. Their rows
are read-only because `/plugin` owns package enablement; press enter on one to
insert `$<name>` into the composer, then add your request and submit it.
Workspace skills keep precedence when names collide.

Orcacode hooks live at
`io.github.okikorg.orcacode/hooks.json`, use direct structured process launches,
and receive one JSON object on stdin. The supported events are
`on_agent_start`, `before_model`, `after_model`, `before_tool`, `after_tool`,
`on_error`, and `on_agent_end`. `before_tool` may return a `continue`, `deny`,
or `rewrite` decision; `after_tool` may replace `output` and `is_error`. Hook
commands run with the same sanitized environment and `PLUGIN_ROOT` / `PLUGIN_DATA`
values as plugin MCP children, without a shell added by Orcacode, and are bounded
to 5 seconds by default (30 seconds maximum):

```json
{
  "version": 1,
  "hooks": {
    "before_tool": [
      {
        "command": "python",
        "args": ["${PLUGIN_ROOT}/scripts/check_tool.py"],
        "timeout_ms": 2000
      }
    ]
  }
}
```

Hook stdout is protocol-only: emit no output for observation hooks, or exactly
one JSON response object for transforming hooks; diagnostics belong on stderr.
Deterministic hook failures stop the current agent run instead of silently
bypassing plugin policy. `on_error` and `on_agent_end` are best-effort because
the native lifecycle does not propagate failures from those terminal observers.
`plugin validate` only parses hooks; explicit `plugin test` executes each hook
once with a representative payload in addition to probing MCP servers.

## Core tools

`orca-harness-tools` gives an agent the ability to act on a machine and hand
results to the model:

- `shell` — run a command, capture stdout/stderr/exit code. Runs on the host
  by default; point it at another machine or container with an `Executor`
  (`Executor::ssh("user@host")`, `Executor::docker_exec("ctr")`) and the
  model drives that target through the same contract. Kills the child on
  cancellation, caps output, enforces a timeout.
- `process` — keep background processes and interactive stdin/stdout sessions
  alive across calls, or wait for a finite long-running command to exit in one
  call so intermediate progress does not require model-driven polling. In the
  interactive CLI, detached processes wake the model once on exit; use
  `notifyOnMatch` at spawn time for a one-shot readiness or important-log wake.
- `pykernel` and `bun_repl` — persistent Python and JavaScript/TypeScript
  compute. Both preserve state across calls, serialize only against themselves,
  and restart explicitly after a timeout. `bun_repl` uses the `bun` executable
  on `PATH`, supports imports and top-level `await`, and never auto-installs
  missing packages.
- `read_file`, `write_file`, `edit_file`, `apply_patch`, `multi_edit`,
  `list_dir`, `grep`, `glob` — all rooted at a `Workspace` that rejects
  absolute paths and `..` escapes. `apply_patch` preflights multi-file
  add/update/delete patches; `multi_edit` preflights ordered exact replacements
  and explicit append operations across existing files.
  Orcacode positions the tools crate's `MutationPreflight` extension before
  Auto admission, so malformed mutation syntax fails cheaply without adding a
  validation hook to `harness-core`. In Auto mode, ordinary workspace writes,
  exact edits, multi-edits, and non-deleting patches proceed through these
  deterministic guards without a second model review; whole-file patch
  deletion remains reviewed.
  Mutations lock every target path: overlapping calls serialize while disjoint
  calls run concurrently.
- `todo_write` — the agent's task list as structured state, shared with the
  host through a cloneable `TodoList` handle so a UI can render the plan
  without parsing it out of the conversation.

`core_tools(&ws)` returns the recommended default set ready to register.

### Read before write

`write_file` replaces a file wholesale, so a model that has not seen the
current contents is not overwriting a file — it is deleting one and
writing another. A `FileGuard` shared by `read_file`, `write_file`, `edit_file`,
`apply_patch`, and `multi_edit` makes `write_file` refuse that:

| Situation                                   | Result                                     |
| :------------------------------------------ | :----------------------------------------- |
| the path does not exist                     | written — a create needs no prior read     |
| read (or written) through this guard, unchanged | written                                |
| exists, never read                          | refused: read it first, or use `edit_file` |
| changed on disk after the read              | refused: read it again                     |

`edit_file`, `apply_patch`, and `multi_edit` work from the current contents and
fail when their expected text is absent, so they do not need a prior read. Their
writes are stamped, so a later `write_file` to the same path is not stranded. A
stamp is the file's own post-write modified time and length, never the
clock.

`core_tools(&ws)` wires a fresh guard. A host that rebuilds its tool set
while one conversation continues — `orcacode` does, on every model
switch, extension toggle, and MCP reload — should pass its own with
`core_tools_with_guard(&ws, &guard)` so what the model read is not
forgotten on every rebuild, and `clear()` it when the conversation resets
(`/clear`). Tools built directly (`WriteFileTool::new(ws)`) are
unguarded; the guard is opt-in for library users.

## Critical extensions

`orca-harness-extensions` — each subscribes only to the hooks it uses, so
registering an unused one costs nothing on the hot path:

- **EventStream** — turns the lifecycle into a typed `HarnessEvent` stream
  delivered to a sink (closure or channel). Tags mirror a standard NDJSON
  union (`assistant_delta`, `reasoning_delta`, `assistant`, `tool_call`,
  `tool_started`, `tool_finished`, `tool_result`, `usage`, `result`, `error`)
  so a host can serialize them directly. Its host-positioned
  `execution_marker()` emits `tool_started` through the existing extension
  chain, keeping preflight timing outside the kernel. This is the main seam
  for building on the harness.
- **ToolPolicy** — allow/deny tool calls before execution (allowlist,
  denylist, or a custom predicate).
- **Truncation** — cap oversized tool outputs to protect the context window
  and any downstream line cap. Paired with the harness's `read_tool_result`
  tool, so the model can page back through the full untruncated output.
- **ToolRetry** / **RetryModel** — retry failing tools (an `around_tool`
  extension) and transient model errors (a `Model` decorator). The CLI wires
  `ToolRetry` to also retry failures core tools report *as data* — a nonzero
  shell exit, an HTTP 5xx from `web_fetch` — while excluding native file
  mutation errors because exact mutation calls must not be replayed after
  deterministic matching, read guards, or possible partial-I/O failures.
- **UsageMeter** — accumulate self-reported token usage across a run,
  readable via a shared handle after it returns.
- **MemoryExtension** — query one embedded SQLite FTS5 store for global and
  current-workspace records. `MemoryModel` adds the bounded provenance-bearing
  fragment to a transient request clone; `MemorySearchTool` and
  `MemoryManageTool` share the same store and scope.

## Quick start

```rust
use orca_harness_core::{Agent, FnTool};
use orca_harness_model_providers::openai::OpenAiModel;
use serde_json::json;

let model = OpenAiModel::new("gpt-4o").api_key(std::env::var("OPENAI_API_KEY")?);
let shell = FnTool::new("shell", "Run a command", json!({"type": "object"}),
    |input, _ctx| async move { Ok(input) });

let agent = Agent::new(model).tool(shell);
let result = agent.run("Fix the failing test").await?;
```

Models can stream: `Model::generate_streaming` emits incremental
`ModelDelta`s (assistant text and reasoning fragments) to subscribed
extensions while the authoritative `ModelResponse` is still the only thing
the loop acts on. Adapters that don't stream need no changes — the default
implementation falls back to `generate`, and the loop only takes the
streaming path when an extension actually subscribes to deltas.

Concurrent tool execution, cancellation, deadlines, step limits, and
deterministic call/result ordering are built into the kernel. Everything
else (memory, MCP, permissions, tracing, retries, sandbox routing, ...)
composes through the `Extension` trait — with subscriptions compiled into
per-event arrays at construction, so unused extensibility approaches zero
cost.

## Concurrency semantics

A tool classifies each call via `Tool::concurrency(&input)`:

| Class                | Behavior                                                                                                              |
| :------------------- | :-------------------------------------------------------------------------------------------------------------------- |
| `Parallel` (default) | safe to run alongside anything                                                                                        |
| `Serial`             | exclusive — nothing else executes while it does                                                                       |
| `Keyed(key)`         | calls sharing a key serialize in call order; unrelated calls continue concurrently (e.g. writes keyed by target path) |

`Limits::max_parallel_tools` can cap simultaneous execution; the default is
unbounded, and tools with a narrower useful width regulate themselves (the
file tools share a bounded filesystem I/O gate). Whatever order tools
*finish* in, the model always sees results in the original call order with
call ids paired.

## Performance

The metric that matters is not binary startup: it is the overhead added
between a model emitting tool calls and those tools doing useful work. The
kernel's goal: **< 1 ms added latency to fan out 100 parallel tool calls,
p99 dispatch overhead < 2 ms** — so even a 0.1 ms filesystem tool barely
notices the harness.

`examples/fanout_probe.rs` measures it directly with no-op tools, where
T0 = dispatch entry, T1 = first tool body started, T2 = last tool body
started:

```bash
cargo run --release --example fanout_probe -- 100 300   # batch size, iterations
```

`./benchmarks/kernel/run.sh` runs that probe across batch sizes, records the
results as JSON and holds them to a budget; `./benchmarks/startup/run.sh` does
the same for the CLI's cold start. See [benchmarks/](benchmarks/).

Measured on a 4-core Linux box (release build, tokio multi-thread):

| Metric                     |    p50 |    p99 |    target |
| :------------------------- | -----: | -----: | --------: |
| 1 call, dispatch (T1−T0)   | 0.4 µs | 0.6 µs |   < 10 µs |
| 10 calls, fan-out (T2−T0)  |  60 µs | 121 µs | < ~100 µs |
| 100 calls, fan-out (T2−T0) | 148 µs | 288 µs |    < 1 ms |
| 100 calls, full round-trip | 183 µs | 326 µs |    < 1 ms |

How the hot path stays that flat:

- **No cross-thread handoff for a single call** — one unit of every batch
  runs inline in the dispatching task after the rest are spawned, so a
  1-call batch is pure function-call overhead (~1 µs round-trip).
- **Synchronization is elided when it cannot constrain the batch** — the
  parallelism semaphore only exists when the batch exceeds
  `max_parallel_tools`; the Serial-exclusivity RwLock only exists when the
  batch contains a `Serial` call.
- **No per-call `ToolCall` clones** — tasks address the batch through one
  shared `Arc<[ToolCall]>`, and each call's input `Value` is moved, not
  copied, into execution.
- Extension hooks compile to per-event arrays; with no subscribers each
  hook site is an empty-slice check.

The residual T1−T0 for large batches (~40 µs here) is tokio waking a parked
worker thread; on a busy server with hot workers it shrinks further. Numbers
scale with core count — re-measure on your target hardware with the probe.

### Real core-tool fan-out

`fanout_probe` measures pure dispatch with no-op tools. The
`orca-harness-tools` probe drives the **actual** `write_file`, `read_file`,
and `shell` tools through the real Dispatcher — real filesystem writes and
real subprocesses — so harness overhead is measured against genuine tool
latency:

```bash
cargo run -p orca-harness-tools --release --example tool_fanout_perf
```

Measured on the same 4-core box (batch of 100, or 64 for subprocesses):

| Case (one model turn)          | wall p50 | throughput | speedup vs serial |
| :----------------------------- | -------: | ---------: | ----------------: |
| 100 × `write_file`, distinct   |   3.1 ms |    32k/sec |                 — |
| 100 × `read_file`, same file   |   1.4 ms |    72k/sec |                 — |
| 64 × `shell`, 20 ms subprocess |    66 ms |    ~1k/sec |         **19.5×** |

The shell case is the headline: 64 subprocesses that would take 1.28 s
serially finish in 66 ms concurrently. Its ~38 ms fan-out overhead is the OS
cost of `fork`/`exec`-ing 64 processes on 4 cores (spawned concurrently),
not harness scheduling — the kernel's own dispatch overhead stays in the
sub-millisecond range the no-op probe shows. For latency-bound tools (HTTP,
remote MCP, subprocesses) the harness turns `sum(latencies)` into
`max(latency)`, which is the whole point of building concurrency into the
kernel.

### Binary size and footprint

`orcacode` ships as a single static binary — no bundled runtime, no wrapper
processes:

| Metric                                                              |      Value |
| :------------------------------------------------------------------ | ---------: |
| Release binary (default profile)                                    |    11.7 MB |
| Release binary (`lto = "fat"`, `codegen-units = 1`, `strip = true`) | **6.3 MB** |
| Idle resident memory (one live session)                             |      ~8 MB |
| Processes at runtime                                                |          1 |

### Compare to

Measurements come from the same Apple Silicon Mac, not vendor claims. Binary
sizes were refreshed on 2026-08-27 and use decimal MB (1 MB = 1,000,000
bytes). Idle RSS was measured on 2026-08-22; every row was a live session
doing nothing, sampled with `ps` over the CLI's process tree after ~10 s idle:

| CLI (version)                                           |                     Binary | Idle RSS (live session) | Processes |
| :------------------------------------------------------ | -------------------------: | ----------------------: | --------: |
| `orcacode` 0.1.0 (this repo)                            |                 **6.3 MB** |               **~8 MB** |         1 |
| `fx` 0.0.5 (for scale)                                  |                     6.4 MB |                  ~21 MB |         1 |
| Grok Build 1.0.5 (`grok`)                               |                   134.3 MB |                 ~90 MB |         1 |
| `pi` 0.84.2 (`@earendil-works/pi-coding-agent`)         |             131 MB install |                 ~211 MB |  1 + node |
| Codex 0.149.0-alpha.4.1 (bundled with ChatGPT)           |    220.5 MB + 57.2 MB host |                 ~340 MB |   up to 3 |
| `prime-agent` 0.7.4                                     |             265 MB install |                 ~400 MB | 1 + node + py |
| `omp` 17.4.2 (`@oh-my-pi/pi-coding-agent`)              |         233 MB + 63 MB bun |                 ~406 MB |   1 + bun |
| Claude Code 2.1.220 (`@anthropic-ai/claude-code`)       |                   256.9 MB |                 ~456 MB |         1 |

Numbers are the core CLI only: any MCP servers configured for a CLI spawn
on top of this at startup (on the measurement machine they added 1–2 GB and
up to a dozen node processes to Codex, omp, and Claude Code alike).

Codex, Claude Code, and omp bundle a JavaScript runtime (Bun/Node); pi runs
on a Node process — hence the order-of-magnitude gaps on both axes. Grok
Build is a native single binary too, but a full product at 134 MB. A
pure-Rust kernel sits next to `fx`, not next to the product CLIs, which
is what makes it cheap to embed as a system's execution primitive
(`ORCA_HARNESS_BIN`) and to run many agents per host.

## Develop

Local development and CI use the current Rust stable toolchain, selected by
`rust-toolchain.toml`. Rustup installs it, including Clippy and rustfmt, when
you run Cargo in this checkout.

```bash
cd orca-harness
cargo test --workspace   # unit + integration tests (fake scripted LLM)
cargo run --example fake_llm   # end-to-end run showing 4-way tool fan-out
cargo run -p orca-harness-tools --example agent_with_tools
                         # full stack: kernel + host tools + extensions
cargo bench              # criterion suite: dispatch latency, fan-out,
                         # extension overhead, keyed scheduling
cargo clippy --workspace --all-targets
./benchmarks/kernel/run.sh   # dispatch + real-tool probes, budget-checked
./benchmarks/startup/run.sh  # orcacode cold start, budget-checked
```

From the repo root: `make harness-test` / `make harness-bench`.

The integration tests drive the kernel end-to-end through
`testing::ScriptedModel`, a fake LLM that replays scripted responses, and
assert the concurrency mechanism directly: barrier tests prove genuine
overlap, probes assert per-key serialization and the parallelism high-water
mark, and cancellation/deadline tests prove propagation into in-flight
fan-out.

## v0.1 scope

Kernel (`harness-core`): Agent, Context, Model, Loop, Dispatcher, Tool,
ToolRegistry, Extension, ExtensionRegistry, Limits, cancellation, typed
errors — plus the OpenAI-compatible adapter and a basic benchmark suite.

Deliberately not in the kernel: durable sessions, workflow DAGs, queues,
planners, distributed scheduling, built-in memory, UI. Those belong to the
surrounding system, to Extensions, or to hosts like the `orcacode` CLI.
