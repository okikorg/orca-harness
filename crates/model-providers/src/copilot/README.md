# Copilot wrapper

`CopilotModel::new(model).api_key(github_token)` exchanges the GitHub token for
an expiring Copilot credential. `base_url`, `max_tokens`, and
`reasoning_effort` are consuming builders. Both core `Model` generation methods
are supported. The registry selects the wire protocol, not model-name heuristics.

The API root override excludes `/v1` (the Messages route adds it). Both explicit
and returned API roots must be HTTPS subdomains of `githubcopilot.com`, on port
443, without userinfo, query, or fragment. Missing `endpoints` uses the registry's
individual-account root. An invalid returned endpoint is rejected even when an
override was supplied.

A per-instance async mutex serializes generation and credential refresh. Tokens
are refreshed within 60 seconds of expiration; refresh failures fail the call
without destroying adapter state. Existing adapters are moved through their
credential builders, never reconstructed during refresh, preserving signed or
encrypted reasoning replay caches. Failed calls are not automatically replayed.

## Security boundary and limitations

- Token exchange uses a dedicated no-redirect client, a 15-second timeout, and
  redacted diagnostics. The GitHub token is never passed to generation adapters.
- Generation still uses the existing adapters' shared HTTP client. Those adapters
  expose no client/redirect-policy injection. Consequently this wrapper cannot
  enforce **no redirects for generation** without changes outside this directory.
  Reqwest's default sensitive-header redirect handling is not a replacement for
  a strict no-redirect policy. Do not treat this implementation as satisfying
  strict end-to-end no-redirect requirements until that adapter hook is added.
- No device login, environment lookup, persistent credential store, enterprise
  domains, token `proxy-ep` fallback, or model-policy enablement is implemented.
- Tests mock token exchange through a private, test-only URL field; production
  callers cannot redirect GitHub token exchange. There is no live Copilot smoke
  test or end-to-end generation/reasoning-replay test in this directory.

Run `cargo test -p orca-harness-model-providers copilot:: --lib` once the host
exports this module. Module/export wiring is intentionally outside this change.
