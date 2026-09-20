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

## Orchestrate mode

Start with `orcacode --orchestrate`, select `/mode orchestrate` in the terminal,
or choose it from the bare `/mode` picker. Leave with `/mode normal`. This mode
is session-local and is not saved to configuration; normal remains the startup
default without a mode flag.

The parent investigates, breaks substantial work into bounded `subagent` tasks
(or interactive `workflow` stages), and synthesizes results. It may use the
read-only allowlist plus `write_file`, `edit_file`, `multi_edit`, and `apply_patch`
for basic edits, including source files. “Basic” is guidance to the model, not
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
