# Orcacode external documentation

A static documentation site for people using `orcacode` (source checkout
version 0.7.0, Apache-2.0). Run these commands from the repository root;
Bun is needed to serve the site, but no build or package install is required.

```bash
bun docs/external/server.ts
# open http://localhost:3030
```

Set `PORT` to use another port:

```bash
PORT=8080 bun docs/external/server.ts
```

The site is intentionally plain HTML, CSS, native browser JavaScript modules, and
inline SVG: no framework is required. The content reflects the CLI and
configuration behavior in this repository. For binary installation on macOS,
Linux, or Windows, see the [repository README](../../README.md#try-it). The Rust
SDK is built from this workspace; it is not yet published on crates.io.

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

## Publishing

The live manual at https://orcapods.ai/orcacode/docs/ is a copy of this
directory in the landing site (`landing/public/orcacode/docs` in the
`okikorg/orca` repository). Keep the two identical, apart from this README and
`server.ts`, which exist only here: edit here, then copy the files across.

```bash
rsync -a --exclude README.md --exclude server.ts docs/external/ ../agent-orc/landing/public/orcacode/docs/
```

## Validation

Run the dependency-free structural checker with Python 3:

```bash
python3 docs/external/check.py
```

It checks registered page fragments, required metadata, navigation parents,
section IDs, duplicate DOM IDs, internal page/section routes, and local asset
paths. It does not fetch external URLs or verify source-code claims. After editing,
also serve the site and check navigation, search, and Copy Markdown in a browser.
The displayed documentation review date is not a release date.

## Included guides

- Install and first session
- Approvals, plan mode, [orchestrate mode](index.html#orchestrate-mode), auto mode, and yolo mode
- Providers (local OpenAI-compatible, OpenAI API, OpenRouter, Vercel AI Gateway,
  CheaperInference, Anthropic, and ChatGPT/Codex), models, and persistent configuration
- Workspace tools, background processes, and output retrieval
- Durable memory, automatic scoped recall, explicit management, and local storage
- Sessions, task lists, and transcript controls
- Complete current tool and extension reference, including mode and availability rules
- Rust SDK guide set with focused subpages for first-agent setup, production host assembly, and background lifecycle management
- Benchmark methodology, checked-in results, regression budgets, MCP retrieval accuracy, and subagent stress evidence
- Subagents, sidekicks, and multi-provider worker routing, MCP servers, and skills (including discovery, scope, and lifecycle diagrams)
- Headless automation and troubleshooting
- Harness architecture, design philosophy, and benchmark methodology/results
