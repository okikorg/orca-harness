# `orca-harness-tool-extensions`

`orca-harness-tool-extensions` contains opt-in integrations that expose external systems as Orca Harness tools. They are separate from the baseline tools because they may start processes, load instruction packages, or make network requests.

## Modules

- `mcp` connects to Model Context Protocol servers, including stdio transports, and catalogs their tools.
- `skills` discovers, validates, installs, and invokes packaged or local skill instructions.
- `agent_plugins` handles plugin manifests, paths, MCP servers, skills, and plugin lifecycle hooks.
- `web` provides policy-aware fetching, search, crawling, and HTML-to-Markdown conversion.
- `plugin_hooks` adapts plugin lifecycle callbacks to core extensions.

Activation is explicit: hosts choose the integrations and pass any filesystem, network, or process policy required by their environment. Keeping these modules optional lets a restricted host use only core tools.

```bash
cargo test -p orca-harness-tool-extensions
cargo run -p orca-harness-tool-extensions --example mcp_connect_probe
```

## Workspace role

This crate is the optional integration boundary: depend on it only when a host needs MCP, skills, plugins, or web access. Network access, subprocess launch, filesystem roots, and plugin activation are all explicit host choices; the crate does not silently enable them.

Related crates: [`orca-harness-core`](../harness-core), [`orca-harness-tools`](../tools), and [`orca-harness-extensions`](../extensions).
