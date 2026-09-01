# `orcacode`

`orcacode` is the reference terminal host for Orca Harness. It wires together the execution kernel, model providers, authentication, baseline and optional tools, lifecycle extensions, session storage, and a Ratatui-based interactive UI.

## Modes

- Interactive mode provides a streaming REPL with markdown rendering, tool approvals, session controls, provider/model pickers, themes, MCP and skills support, and live inspection.
- Headless mode (`-p`/`--prompt`) runs one prompt and streams the answer to stdout.
- `--json` emits NDJSON harness events for scripting and integrations.
- `--plan` restricts writes to the plan area; `--normal`, `--auto`, and `--yolo` select approval behavior.
- `plugin` manages Agent Plugin packages.

## Usage

```bash
cargo run --release -p orcacode
cargo run --release -p orcacode -- -p "your prompt"
cargo run --release -p orcacode -- --json -p "your prompt"
cargo run --release -p orcacode -- --plan -p "design the change"
```

Common options include `--model`, `--base-url`, `--api-key`, `--openrouter`, `--workspace`, `--tools`, `--bare`, `--max-steps`, `--continue`, `--resume`, and `--no-session`. Run `orcacode --help` for the complete list.

Provider settings, themes, API keys, approval rules, and selected models are persisted under `~/.config/orcacode`; flags and environment variables take precedence. Sessions are stored per workspace and can be listed, resumed, rewound, or forked from the TUI. OpenAI Codex can use the device-login flow and does not require an API key.

```bash
cargo test -p orcacode
```

## Workspace role

The binary is a host and reference application, not the execution kernel. It supplies the terminal UX and chooses the default provider, tools, extensions, approval behavior, and session storage. Embed Orca Harness through [`orca-harness-sdk`](../sdk) or the lower-level crates when you need a different host.

Related documentation: the workspace [README](../../README.md) and [crate diagram](../../docs/crate-diagram.md).
