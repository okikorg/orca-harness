# Orcacode Agent Plugins v1

## Goal

Implement Orcacode as an Agent Plugins 1.0 client for portable Agent Skills and MCP over stdio. A plugin is a local directory with a canonical `plugin.json`, optional `skills/`, and optional `mcp.json`; Orcacode registers the canonical path, installs it disabled, loads enabled Skills through the existing skill catalog, and starts enabled MCP servers through the existing MCP catalog and tool-policy path.

Authoritative contract: [Agent Plugins Specification 1.0.0](https://agent-plugins.org/specification). The specification and its canonical schemas govern any conflict in this plan.

## Global constraints

- Do not change `harness-core`, the native Rust extension lifecycle, WASM support, the Rust SDK, standalone Skills semantics, or the standalone `/mcp` UX.
- Do not invent an Orcacode plugin manifest. Use root `plugin.json` and optional root `mcp.json` only.
- V1 supports Agent Plugins MCP servers using `type: "stdio"` only. Report and skip `streamable-http` and `sse` entries without blocking valid siblings.
- Do not install dependencies, run package setup scripts, fetch schemas at runtime, provide a marketplace, or add remote installation. Portable v1 remains Skills and MCP only; the later hook addendum below is an Orcacode client extension.
- Plugin processes are child processes, not a security sandbox. They use a sanitized base environment but retain the OS permissions of the user.
- Preserve existing standalone command-string MCP configuration and its inherited-environment behavior.
- CLI plugin management exits before provider, model, session, or TUI initialization. TUI plugin management saves the same next-process state without hot-loading plugin processes.
- Preserve unrelated configuration fields and unrelated worktree changes. Keep changes cohesive and avoid drive-by cleanup.
- Do not start the Orcacode TUI or a persistent server during verification.

## Task 1: Agent Plugin parsing and validation

Add `crates/tool-extensions/src/agent_plugins.rs` (or a focused module directory) and export it from `tool-extensions`.

Provide:

```rust
pub struct AgentPlugin {
    pub name: String,
    pub version: Option<String>,
    pub root: PathBuf,
    pub skills: Discovered,
    pub mcp_servers: Vec<PluginMcpServer>,
    pub warnings: Vec<PluginWarning>,
}

pub struct PluginMcpServer {
    pub id: String,
    pub plugin_name: String,
    pub server_name: String,
    pub launch: StdioLaunch,
}

pub fn load_agent_plugin(root: &Path, plugin_data: &Path) -> Result<AgentPlugin, PluginError>;
```

The data-directory argument is an allowed internal adjustment to the earlier sketch because `PLUGIN_DATA` expansion and containment cannot be correct without the client-owned path.

Implement the locally shipped Agent Plugins 1.0.0 rules:

- Require a filesystem-resolved plugin root and regular root `plugin.json` that remains inside it.
- Require the canonical plugin schema URL and validate all standard field types. Unknown top-level fields and non-object `extensions` are warnings and are ignored; other manifest violations reject the plugin.
- Enforce names of 1-64 lowercase `a-z`, digits, hyphens, and periods; alphanumeric at both ends; no `--` or `..`.
- Discover only immediate children of `skills/` containing an exact regular `SKILL.md`. Validate Agent Skills names, descriptions, directory-name matching, and plugin-root containment; skip and report invalid siblings without blocking MCP. Missing or empty `skills/` is valid.
- Detect client extension namespaces and warn that Orcacode v1 does not load them.
- If present, require `mcp.json` to be a regular in-root file, with only canonical `$schema` and `mcpServers` top-level fields. An invalid MCP document disables MCP for that plugin but does not invalidate the manifest.
- Validate each MCP server independently. A stdio server has closed fields `type`, `command`, optional string-array `args`, optional string-map `env`, and optional `cwd`.
- `command` is one non-empty bare executable token or a plugin-relative path beginning `./`. Never expand placeholders in it. Resolve plugin-relative commands inside the canonical plugin root, including symlink containment.
- Expand every exact `${PLUGIN_ROOT}` and `${PLUGIN_DATA}` occurrence once, non-recursively, in args, env values, and cwd only. Leave other placeholder-like text literal.
- Reject reserved `PLUGIN_ROOT` and `PLUGIN_DATA` env keys.
- Default cwd to plugin root. Explicit cwd must be `./...`, `${PLUGIN_ROOT}` rooted, or `${PLUGIN_DATA}` rooted and must remain within its corresponding canonical boundary, including symlink escapes.
- Report unsupported `streamable-http` and `sse` servers as warnings and keep valid stdio siblings.
- Generate deterministic IDs as `plugin__<normalized-plugin-name>__<normalized-server-name>` and reject collisions before affected servers start. Normalization must be deterministic and tested.

Cover valid minimal manifests and stdio config, unsupported/missing schemas, names and unknown fields, isolated invalid entries, unsupported transports, placeholder behavior, reserved env overrides, lexical and symlink escapes, normalized IDs, and collisions.

## Task 2: Structured stdio MCP launches

Generalize the launcher in `crates/tool-extensions/src/mcp/`:

```rust
pub struct StdioLaunch {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub environment: ProcessEnvironment,
}

pub enum ProcessEnvironment {
    Inherit,
    Sanitized,
}
```

Add `McpClient::connect_stdio(server_name, &StdioLaunch)`. It must invoke the executable directly, pass args separately, apply cwd, and either inherit the ambient environment or clear it and preserve only runtime essentials (`PATH`, home/profile, temp, locale, platform system variables, and TLS certificate paths) before overlaying manifest env. Never use a shell.

Keep `McpClient::connect(server_name, command_string)` as a compatibility wrapper with its existing whitespace splitting and inherited environment.

Test structured args/cwd/env using a fake MCP server, sanitized secret omission, compatibility inheritance, reserved plugin variables supplied by the caller, cancellation, deadlines, handshake failures, and child cleanup.

## Task 3: Persistent registration and plugin CLI

Extend the existing config JSON with:

```json
{
  "plugins": {
    "rl-tools": {
      "root": "/absolute/canonical/path/to/rl-tools",
      "enabled": false
    }
  }
}
```

Missing `plugins` is backward compatible. Preserve unknown and unrelated fields. Sort names for deterministic list/startup. Store persistent data at `<config-dir>/plugin-data/<plugin-name>/`; create it only immediately before an enabled plugin server is started. Uninstall removes registration only and retains source and plugin data.

Refactor CLI parsing to an early invocation boundary:

```rust
enum Invocation {
    Run(Config),
    Plugin(PluginCommand),
}
```

`plugin` must be the first positional token. Support:

```text
orcacode plugin init <name> --python|--py|--typescript|--ts
orcacode plugin validate [PATH]
orcacode plugin test [PATH]
orcacode plugin install [PATH]
orcacode plugin list
orcacode plugin inspect <name>
orcacode plugin enable <name>
orcacode plugin disable <name>
orcacode plugin uninstall <name>
```

PATH defaults to current directory. `validate` is static. `test` explicitly launches every valid stdio server, handshakes, lists tool names, and shuts down. `install` validates and registers the canonical path disabled without copying. Same name/same path is idempotent; same name/different path fails. `enable` revalidates before saving. `uninstall` prints the retained plugin-data path. Changes take effect next launch.

Keep parsing/output/config operations and templates in a focused CLI plugin module. Test every command/state transition, unrelated config preservation, source/data preservation, static validation non-execution, runtime probing, and early exit before provider/TUI work.

## Task 4: Python and TypeScript scaffolds

`init` fails rather than overwriting a non-empty target and writes source/config only.

Python structure:

```text
<name>/plugin.json
<name>/mcp.json
<name>/pyproject.toml
<name>/README.md
<name>/.gitignore
<name>/skills/.gitkeep
<name>/src/<normalized_package>/__init__.py
<name>/src/<normalized_package>/server.py
<name>/tests/test_server.py
```

Use Python 3.11+, `uv`, official stable-major MCP Python SDK, and a minimal `echo` tool. The MCP launch uses `uv run --project ${PLUGIN_ROOT} ...`, with uv environment/cache paths in `${PLUGIN_DATA}`. Orcacode never installs dependencies.

TypeScript structure:

```text
<name>/plugin.json
<name>/mcp.json
<name>/package.json
<name>/package-lock.json
<name>/tsconfig.json
<name>/README.md
<name>/.gitignore
<name>/skills/.gitkeep
<name>/src/index.ts
<name>/test/server.test.ts
```

Use Node 20+, stable `@modelcontextprotocol/sdk` 1.x, TypeScript, Vitest, and esbuild. Build `dist/server.mjs`; runtime and `plugin test` require the build. Generate a deterministic lockfile without installing dependencies.

Both manifests use canonical Agent Plugins 1.0.0 schema URLs and create a tracked, intentionally empty `skills/` component. Every MCP entry explicitly includes `"type": "stdio"`; command is a bare executable and args/cwd carry placeholders according to the standard.

Test aliases, layouts, no overwrite, and validation of generated manifests.

## Task 5: Runtime composition

At interactive and headless startup, load enabled plugin Skills through the existing Skills catalog and reconcile standalone MCP config plus plugin servers through the existing MCP manager/catalog:

1. Load standalone servers.
2. Load registered enabled plugins sorted by plugin name.
3. Build structured launches with deterministic reserved IDs.
4. Create plugin data only before launch.
5. Connect each server independently, report scoped diagnostics, and keep healthy siblings.
6. Register tools through the existing MCP catalog so pagination, selection, invalidation, cancellation, deadlines, `PlanGate`, approval gates, and subagent paths remain shared.

`/mcp` continues to edit standalone servers only and also lists enabled plugin servers as read-only rows with package provenance and live connection state. `/skills` likewise lists loaded plugin Skills as read-only rows; enter inserts `$<skill-name>` through the existing explicit invocation path, while package enablement remains in `/plugin`. `/plugin` uses the shared picker to list registrations, run the same management operations, and distinguish saved state from Skills and tools loaded in the current process; changes remain next-launch only. A disabled plugin contributes no Skills or tools. Workspace Skills retain precedence on name collisions. A missing/moved root, invalid Skill, or failed server does not block healthy siblings or another plugin. Interactive and headless construction must use the same shared state.

Test enabled/disabled visibility, broken-plugin isolation, moved roots, normalized collisions, shared catalog search/select/call, and plan/approval behavior through existing gates where practical.

## Task 6: Documentation and verification

Add a Plugins page under the public Extend documentation, navigation metadata, and a concise README section. Explain plugin versus native extension/WASM/SDK, directory layout, both scaffold workflows, static validate versus executable test, linked install, disabled-by-default state, no dependency installation, no sandbox guarantee, next-process activation, stderr/stdout discipline, cancellation, bounded work, stable schemas, secrets guidance, and `PLUGIN_DATA` uses.

Document RL patterns: separate create/reset/step/close tools, handles instead of runtime object serialization, explicit schemas, artifact paths for large tensors/checkpoints, idempotent reset/close, cancellation checks, and one MCP server unless process isolation is needed.

Exercise both generated projects in temporary directories without starting the TUI or a persistent server:

- generate each scaffold;
- explicitly install dependencies as a developer step;
- run unit tests;
- build TypeScript;
- run `plugin validate` and `plugin test`;
- install, enable, load through actual runtime construction, disable, and uninstall.

Run focused checks, then:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
sh ci/check-source-size.sh
git diff --check
```

Finish with a fresh read-only review of the final diff. Require an evidence-backed `PASS` for scope, Agent Plugins compatibility, environment/path security boundaries, existing MCP compatibility, and DRY/YAGNI adherence.

## Orcacode hook extension addendum

Agent Plugins 1.0 deliberately leaves hooks outside its two portable component types. Orcacode implements hooks through the file-only reverse-domain namespace `io.github.okikorg.orcacode`, discovered at `io.github.okikorg.orcacode/hooks.json`. The client-owned format is independently versioned and uses structured direct executable launches, the sanitized plugin environment, `PLUGIN_ROOT` and `PLUGIN_DATA`, bounded JSON stdin/stdout, and isolated static validation. Enabled hooks adapt onto the existing native `Extension` lifecycle without changing `harness-core`; they run in interactive, headless, and subagent construction. Scaffolded hook files are empty, so portable-only plugins incur no lifecycle subscriptions or subprocess work.

## Delivery rulings

- Ruling: the Agent Plugins 1.0.0 specification overrides the earlier example that omitted `type: "stdio"` and used placeholders as if they applied to `command`. Cost if wrong: generated packages could be rejected by conformant clients.
- Ruling: `load_agent_plugin` receives the plugin-data path (or an equivalent resolver) because validation must expand and contain `${PLUGIN_DATA}` before launch. Cost if wrong: the public internal signature differs from the initial sketch, but it avoids validating against a fabricated boundary.
