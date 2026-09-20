# Orcacode external documentation

A dependency-free, static documentation site for people using `orcacode`.
Run these commands from the repository root.

```bash
bun docs/external/server.ts
# open http://localhost:3030
```

Set `PORT` to use another port:

```bash
PORT=8080 bun docs/external/server.ts
```

The site is intentionally plain HTML, CSS, native browser JavaScript modules, and
inline SVG: no build, package install, or framework is required. The content
reflects the CLI and configuration behavior in this repository.

## Content structure

Each manual page body is a plain HTML fragment under `pages/`. Edit the matching
`pages/<id>.html` file when that page changes. `pages/index.json` contains the
small, ordered navigation metadata, while `app.js` contains only shared
rendering, loading, search, navigation, and interaction behavior. Add or reorder
pages in the JSON registry. To display a page as a nested navigation entry, add
`"parent": "<parent-page-id>"`; keep the child next to its parent and in the same
navigation group. The relationship is presentational—the child still has its own
hash route, searchable body, outline, and Copy Markdown action. The browser fetches
the registry and fragments directly, so this separation adds no build step. The shared `markdown.js`
serializer powers each page’s **Copy Markdown** action from the same HTML source;
there are no parallel `.md` files to keep synchronized.

The architecture and concurrency schematics remain inline with their owning
page and use `diagrams.css`, so page copy and its visual explanation can be
audited together.

## Included guides

- Install and first session
- Approvals, plan mode, [orchestrate mode](index.html#orchestrate-mode), auto mode, and yolo mode
- Providers, models, and persistent configuration
- Workspace tools, background processes, and output retrieval
- Durable memory, automatic scoped recall, explicit management, and local storage
- Sessions, task lists, and transcript controls
- Complete current tool and extension reference, including mode and availability rules
- Rust SDK guide set with focused subpages for first-agent setup, production host assembly, and background lifecycle management
- Benchmark methodology, checked-in results, regression budgets, MCP retrieval accuracy, and subagent stress evidence
- Subagents and multi-provider worker routing, MCP servers, and skills (including discovery, scope, and lifecycle diagrams)
- Headless automation and troubleshooting
- Harness architecture, design philosophy, and benchmark methodology/results
