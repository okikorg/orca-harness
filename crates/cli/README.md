# `orcacode`

`orcacode` is the reference terminal host for Orca Harness. It wires together the execution kernel, model providers, authentication, baseline and optional tools, lifecycle extensions, session storage, and a Ratatui-based interactive UI.

## Modes

- Interactive mode provides a streaming REPL with markdown rendering, tool approvals, session controls, provider/model pickers, themes, MCP and skills support, and live inspection.
- Headless mode (`-p`/`--prompt`) runs one prompt and streams the answer to stdout.
- `--json` emits NDJSON harness events for scripting and integrations.
- `--plan` restricts writes to the plan area; `--orchestrate` delegates substantial work and permits basic edits; `--normal`, `--auto`, and `--yolo` select approval behavior.
- `plugin` manages Agent Plugin packages.

## Usage

```bash
cargo run --release -p orcacode
cargo run --release -p orcacode -- -p "your prompt"
cargo run --release -p orcacode -- --json -p "your prompt"
cargo run --release -p orcacode -- --plan -p "design the change"
```

Session modes are selected with `/mode` interactively or startup flags; when
multiple flags are present, precedence is plan, orchestrate, normal, auto, yolo.
`--orchestrate` does not bypass approvals: headless `subagent` calls and gated edits
still need `--auto-approve`. Interactive plan writes can be approved separately
for implementation, which switches to normal mode and starts a follow-up turn.
Interactive plan mode also allows `shell`, but every command asks for approval,
even one you always allow in normal mode; headless `--plan` and workers keep
`shell` denied.

Interactive MCP initialization and reload run in the background; local tools
remain usable while connections are pending. Completed reloads publish tools
at a worker command boundary and preserve local processes and interpreter
state. Headless runs await initialization before running the prompt. See the
workspace [MCP documentation](../../README.md) for configuration and limits.

Common options include `--model`, `--base-url`, `--api-key`, `--openrouter`, `--workspace`, `--tools`, `--bare`, `--max-steps`, `--continue`, `--resume`, and `--no-session`. Run `orcacode --help` for the complete list.

Provider settings, themes, API keys, approval rules, and selected models are persisted under `~/.config/orcacode`; flags and environment variables take precedence. Vercel AI Gateway uses `AI_GATEWAY_API_KEY` and its OpenAI-compatible endpoint. Sessions are stored per workspace and can be listed, resumed, rewound, or forked from the TUI. OpenAI Codex can use the device-login flow and does not require an API key.

```bash
cargo test -p orcacode
```

## Headless engine contract

A host can drive `orcacode` as one process per turn:

```bash
orcacode -p - --json --mcp-config servers.json < prompt.txt          # new session
orcacode --resume SESSION_ID -p - --json --mcp-config servers.json < next.txt
```

Headless flags:

| Flag | Meaning |
| :-- | :-- |
| `-p TEXT`, `-p -`, `--prompt-file PATH` | Prompt inline, from stdin to EOF, or from a file. Exactly one. An empty prompt is an error. |
| `--json` | NDJSON events on stdout, one object per line. Diagnostics stay on stderr. |
| `--resume ID` | Continue a recorded session; use the `sessionId` from a previous run. A unique prefix also works. |
| `--mcp-config JSON\|PATH` | MCP servers for this run only, added to the saved ones; never written to `config.json`. A run server replaces a saved server of the same name (a notice goes to stderr). Not allowed with `--bare`. |
| `--approvals deny\|stdio` | `deny` (default): gated tools are refused unless `--auto-approve`/`--yolo`. `stdio`: ask the caller (requires `--json`; use `--prompt-file`, not `-p -`). |
| `--read-only` | No writes anywhere (stricter than `--plan`, which still writes `docs/plan/`), no compute, delegation or memory writes. MCP tools stay usable. `shell` runs only with `--approvals stdio`, asked every time. `--auto-approve` and `--yolo` do not widen it. |
| `--image PATH` | Attach a png, jpg, gif or webp file (up to 20 MiB) to the prompt. Repeatable. A bad file fails the run before it starts. |
| `--max-steps N` | Model invocations for the run (default 48). |

`--mcp-config` takes inline JSON (a value starting with `{`) or a file path. Either
the bare map or the `mcpServers` wrapper works. The command runs directly, with no
shell: paths and arguments may contain spaces. `env` is merged over the inherited
environment; `args`, `env` and `cwd` are optional.

```json
{"docs": {"command": "/opt/my tools/server", "args": ["--root", "a b"], "env": {"TOKEN": "x"}, "cwd": "/tmp"}}
```

Remote MCP tools are named `mcp__<server>__<tool>` and are loaded through the
`mcp_search_tools`, `mcp_select_tool` and `mcp_features` interface tools. A
`--tools` allowlist must name the interface tools and the remote tools it wants.

### Providers and models

`--provider LABEL` picks the provider for one invocation (headless runs,
`--list-models`, `--list-providers`) and wins over the saved provider. Labels:
`openrouter`, `vercel`, `cheaperinference`, `openai`, `openai-codex`, `anthropic`,
`local`. Keys and logins resolve as usual (flag, environment, saved config, OAuth
login); nothing is written to `config.json`. Without `--model` the provider's saved
model is used, otherwise its default. `ORCA_BASE_URL` is ignored when `--provider`
is given; an explicit `--base-url` still applies. An unknown label, or `--provider`
combined with `--openrouter`/`--anthropic` naming a different provider, exits 1.

`orcacode --list-providers --json` prints one document and makes no network calls:

```json
{"active":"openai-codex","providers":[{"label":"openrouter","name":"OpenRouter","auth":"api_key","configured":true,"model":"openrouter/auto"}]}
```

`auth` is `api_key`, `oauth` or `none`. `configured` means a key in the environment
or config (`api_key`), a stored login (`oauth`), and is always true for `local`.
`model` is the saved model for that provider or its default. Key values are never
printed.

`orcacode --provider openrouter --list-models --json` prints the catalog; a failed
listing exits non-zero with the error on stderr:

```json
{"provider":"openrouter","models":[{"id":"...","name":"Display name","contextWindow":131072,"efforts":["low","high"]}]}
```

`name`, `contextWindow` and `efforts` are `null` when the provider does not publish
them. Without `--json` the plain-text listing is unchanged.

### Events

Every line on stdout is one JSON object with a `type`. The first is `metadata`
and the last is `summary`; harness events come in between.

```json
{"type":"metadata","provider":"local","model":"m","effort":null,"promptCache":true,"maxOutputTokens":null,"bare":false,"tools":null,"sessionId":"<session id>","resumed":false,"readOnly":false,"approvals":"deny","mcpServers":["docs"]}
{"type":"agent_start"}
{"type":"assistant_delta","text":"..."}
{"type":"reasoning_delta","text":"..."}
{"type":"tool_input_delta","text":"..."}
{"type":"assistant","message":"..."}
{"type":"tool_call","tool_call_id":"c1","tool_name":"shell","input":{"command":"ls"}}
{"type":"approval_request","id":"approval-1","tool_call_id":"c1","tool_name":"shell","input":{"command":"ls"}}
{"type":"tool_started","tool_call_id":"c1","tool_name":"shell"}
{"type":"tool_finished","tool_call_id":"c1","tool_name":"shell","is_error":false}
{"type":"tool_result","tool_call_id":"c1","tool_name":"shell","output":"...","is_error":false}
{"type":"usage","usage":{"inputTokens":10,"outputTokens":5,"cacheReadTokens":0,"cacheCreateTokens":0}}
{"type":"result","message":"final answer"}
{"type":"error","message":"..."}
{"type":"summary","status":"success","sessionId":"<session id>","provider":"local","model":"m","effort":null,"durationMs":1234,"usage":{"inputTokens":10,"outputTokens":5,"cacheReadTokens":0,"cacheCreateTokens":0},"modelSteps":2,"toolCalls":1,"modelRetries":0}
```

- `sessionId` is the recorded session's full id, for new and resumed runs alike;
  it is `null` only with `--no-session`. `resumed` is true for `--resume`,
  `--continue` and `orcacode resume ID`.
- `summary.status` is `success`, `max_steps` (the `--max-steps` budget ran out),
  `cancelled` (SIGINT/SIGTERM), or `error`. The exit code is 0 only for `success`.
- `approval_request` appears only with `--approvals stdio`. Answer on stdin with one
  line per request, in any order:

  ```json
  {"type":"approval","id":"approval-1","decision":"allow"}
  ```

  `decision` is `allow`, `deny`, or `always` (allow that tool for the rest of this
  run; never saved, and never applies to read-only `shell`). Requests can be open
  concurrently and are matched by `id`; subagent children ask the same way.
  Malformed lines are ignored with a stderr warning. EOF on stdin denies every
  open and later request. The process exits when the run ends even if stdin is
  still open.

## Orchestrate mode

Start with `orcacode --orchestrate`, select `/mode orchestrate` in the terminal,
or choose it from the bare `/mode` picker. Leave with `/mode normal`. This mode
is session-local and is not saved to configuration; normal remains the startup
default without a mode flag.

The parent investigates, breaks substantial work into bounded `subagent` tasks
(or interactive `workflow` stages), and synthesizes results. It may use the
read-only allowlist plus `write_file` and `edit_file` for basic edits, including source files. “Basic” is guidance to the model, not
an enforced line-count limit or the plan-mode `docs/plan/` write fence. Parent
`shell`, `process`, compute, MCP, and unknown tools are denied before approval;
delegate execution and substantial implementation instead of retrying them.

Workers retain their own registered tools and existing approval behavior;
ordinary worker calls do not gain interactive approval prompts. Interactive
workers view orchestrate as normal, but switching to plan restricts their next
tool calls too. Parent gated calls still need approval. For headless delegation:

```bash
orcacode --orchestrate --auto-approve -p "Delegate focused tests and summarize the results"
```

`--auto-approve` permits gated calls without removing the parent allowlist.
Combining `--orchestrate` with `--auto` or `--yolo` does not bypass it either:
orchestrate wins those combinations; `--plan` wins over orchestrate. Keep
`subagent` enabled: `--bare` omits it, and a `--tools` filter must include it.
The headless host does not register `workflow`.

Use `/subagents` for worker routing, preferred models, and budgets; these are
separate from session mode. In particular, the `auto` worker route is not
`/mode auto`. See the [Orchestrate mode manual](../../docs/external/index.html#orchestrate-mode)
and [worker configuration](../../docs/external/index.html#subagents).

## Workspace role

The binary is a host and reference application, not the execution kernel. It supplies the terminal UX and chooses the default provider, tools, extensions, approval behavior, and session storage. Embed Orca Harness through [`orca-harness-sdk`](../sdk) or the lower-level crates when you need a different host.

Related documentation: the workspace [README](../../README.md) and [crate diagram](../../docs/crate-diagram.md).
