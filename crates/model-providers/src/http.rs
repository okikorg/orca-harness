//! One HTTP client for the whole process.
//!
//! `reqwest` loads the operating system's trust store when a client is built,
//! because the workspace enables `rustls-tls-native-roots` so a corporate root
//! is trusted like any other. That load costs on the order of 100ms and is
//! pure CPU, and every model construction used to pay it: the cold-start path
//! built one, each agent rebuild built another, and every subagent model
//! choice built one more. The roots are identical every time, so the client is
//! built once and cloned after that — a `reqwest::Client` is an `Arc` around
//! its connection pool, so clones share the pool as well as the roots.
//!
//! Per-call settings that used to justify a separate client belong on the
//! request instead: [`reqwest::RequestBuilder::timeout`] overrides the
//! client's timeout for one request.

use std::sync::OnceLock;
use std::time::Duration;

/// The default per-request ceiling for catalog fetches and other one-shot
/// calls that used to build their own client.
pub const CATALOG_TIMEOUT: Duration = Duration::from_secs(15);

static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// The shared client, built on first use.
///
/// Falls back to `Client::new()` if the builder fails, which keeps the
/// signature infallible for constructors that cannot report an error.
pub fn client() -> reqwest::Client {
    CLIENT
        .get_or_init(|| reqwest::Client::builder().build().unwrap_or_default())
        .clone()
}

/// Build the shared client now, off the critical path.
///
/// Startup calls this so the trust-store load overlaps with the rest of the
/// cold-start work instead of blocking the first model construction.
pub fn warm() {
    std::thread::spawn(|| {
        let _ = client();
    });
}
