# Orcacode external documentation

A dependency-free, static documentation site for people using `orcacode`.

```bash
bun docs/server.ts
# open http://localhost:3030
```

Set `PORT` to use another port:

```bash
PORT=8080 bun docs/server.ts
```

The site is intentionally plain HTML, CSS, native browser JavaScript modules, and
inline SVG: no build, package install, or framework is required. The content
reflects the CLI and configuration behavior in this repository.

## Content structure

Each manual page body is a plain HTML fragment under `pages/`. Edit the matching
`pages/<id>.html` file when that page changes. `pages/index.json` contains the
small, ordered navigation metadata, while `app.js` contains only shared
rendering, loading, search, navigation, and interaction behavior. Add or reorder
pages in the JSON registry. The browser fetches the registry and fragments
directly, so this separation adds no build step. The shared `markdown.js`
serializer powers each page’s **Copy Markdown** action from the same HTML source;
there are no parallel `.md` files to keep synchronized.

The architecture and concurrency schematics remain inline with their owning
page and use `diagrams.css`, so page copy and its visual explanation can be
audited together.

## Included guides

- Install and first session
- Approvals, plan mode, and yolo mode
- Providers, models, and persistent configuration
- Workspace tools, background processes, and output retrieval
- Durable memory, automatic scoped recall, explicit management, and local storage
- Sessions, task lists, and transcript controls
- Complete current tool and extension reference, including mode and availability rules
- Rust SDK embedding guide covering setup, models, tool presets, runs, sessions, memory, skills, MCP, recovery, errors, and runnable examples
- Benchmark methodology, checked-in results, regression budgets, MCP retrieval accuracy, and subagent stress evidence
- Subagents and multi-provider worker routing, MCP servers, and skills (including discovery, scope, and lifecycle diagrams)
- Headless automation and troubleshooting
- Harness architecture, design philosophy, and benchmark methodology/results
