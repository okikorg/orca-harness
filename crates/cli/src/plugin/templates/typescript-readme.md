# {{NAME}}

Node 20+ Agent Plugin scaffold using the stable 1.x MCP TypeScript SDK.
The tracked `skills/` directory is intentionally empty; add standard
`skills/<name>/SKILL.md` packages when the plugin needs reusable guidance.
The empty `io.github.okikorg.orcacode/hooks.json` is an optional Orcacode
client extension; Agent Plugins 1.0 itself standardizes only Skills and MCP.

Orcacode does not install dependencies. Install, test, and build explicitly:

```sh
npm ci
npm test
npm run build
orcacode plugin validate .
orcacode plugin test .
```

`orcacode plugin test .` fails until `dist/server.mjs` has been built.
