# Copilot wrapper

`CopilotModel::new(model).api_key(github_token)` exchanges the GitHub token for
an expiring Copilot credential. `base_url`, `max_tokens`, and
`reasoning_effort` are consuming builders. Both core `Model` generation methods
are supported. The first generation discovers the authenticated API root's `/models` catalog
and selects a protocol only from that model's advertised `supported_endpoints`.
Preference is `/v1/messages`, `/responses`, then `/chat/completions`; unknown IDs,
missing capabilities, and unsupported endpoints produce actionable errors.
There are no model-name heuristics or static registry routes.

`models().await` returns `Vec<ModelInfo>` for the authenticated catalog (cached
per instance). Names, context windows, and reasoning efforts are populated only
when advertised; pricing and other unknown metadata remain `None`. Discovery
does not require the constructor's model ID to exist. The wire schema follows
[VS Code Copilot's endpoint reference](https://github.com/microsoft/vscode-copilot-chat/blob/main/src/platform/endpoint/common/endpointProvider.ts),
including top-level `supported_endpoints` and nested `capabilities` metadata.

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

- Token exchange and catalog discovery use the shared no-redirect client, a 15-second timeout, and
  redacted diagnostics (discovery errors never echo the response body). The GitHub token is never passed to discovery or generation adapters.
- Generation uses the provider adapters' shared no-redirect HTTP client. Token
  exchange, discovery, and generation all reject redirects rather than relying
  on sensitive-header stripping.
- No device login, environment lookup, persistent credential store, enterprise
  domains, token `proxy-ep` fallback, or model-policy enablement is implemented.
- Tests mock token exchange and discovery through private test-only URL fields;
  production callers cannot bypass endpoint validation. Arbitrary IDs exercise
  all three adapter selections, metadata absence, unknown IDs/capabilities,
  refresh, redaction, and discovery redirects. There is no live Copilot smoke
  test or end-to-end generation/reasoning-replay test in this directory.

Run `cargo test -p orca-harness-model-providers copilot:: --lib` once the host
exports this module. Module/export wiring is intentionally outside this change.
