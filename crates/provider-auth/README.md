# `orca-harness-provider-auth`

`orca-harness-provider-auth` defines the narrow, provider-neutral authentication boundary used by model adapters. It keeps credential acquisition, refresh, and storage decisions out of the protocol and execution crates.

## API

- `BearerCredential` contains an access token and optional expiry timestamp.
- `CredentialSource` asynchronously supplies the current credential or refreshes it.
- `StaticCredential` adapts an already-issued bearer token for simple hosts and tests.
- `CredentialError` and `CredentialErrorKind` distinguish missing, malformed, expired, refresh, and storage failures.

Hosts own login flows and persistence. A provider adapter consumes a `CredentialSource`; this crate does not read environment variables, write files, or implement a provider-specific login flow.

```rust,no_run
use orca_harness_provider_auth::{BearerCredential, StaticCredential};

let source = StaticCredential(BearerCredential {
    access_token: "token".into(),
    expires_at: None,
});
# let _ = source;
```

```bash
cargo test -p orca-harness-provider-auth
```

## Workspace role

This crate is intentionally small and provider-neutral. Model adapters depend on it, while hosts such as `orcacode` provide the real login and persistence implementation. Do not put API-key discovery or credential files in this crate.

Related crate: [`orca-harness-model-providers`](../model-providers).
