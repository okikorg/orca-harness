# {{NAME}}

Python 3.11+ Agent Plugin scaffold using the official MCP Python SDK.
The tracked `skills/` directory is intentionally empty; add standard
`skills/<name>/SKILL.md` packages when the plugin needs reusable guidance.
The empty `io.github.okikorg.orcacode/hooks.json` is an optional Orcacode
client extension; Agent Plugins 1.0 itself standardizes only Skills and MCP.

Orcacode does not itself run dependency installers. Install and test the
checkout explicitly for development:

```sh
uv sync --dev
uv run pytest
orcacode plugin validate .
orcacode plugin test .
```

The MCP launch redirects uv's environment and cache beneath `${PLUGIN_DATA}`.
That boundary is separate from the checkout's development environment, so the
generated `uv` child may resolve dependencies into `${PLUGIN_DATA}` on the
first `orcacode plugin test` or enabled start.

For offline use, successfully run `orcacode plugin test .` while online
before enabling the plugin, or explicitly populate the same managed uv
environment and cache beneath `${PLUGIN_DATA}`.
