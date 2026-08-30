# {{NAME}}

Node 20+ Agent Plugin scaffold using the stable 1.x MCP TypeScript SDK.

Orcacode does not install dependencies. Install, test, and build explicitly:

```sh
npm ci
npm test
npm run build
orcacode plugin validate .
orcacode plugin test .
```

`orcacode plugin test .` fails until `dist/server.mjs` has been built.
