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
/// Provider credentials may use custom headers that reqwest does not strip
/// across origins. Never follow redirects, including for discovery requests.
/// Initialization fails closed instead of falling back to an unsafe client.
pub fn client() -> reqwest::Client {
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("initialize no-redirect provider HTTP client")
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[tokio::test]
    async fn custom_credentials_are_never_forwarded_on_redirect() {
        for status in [301, 302, 303, 307, 308] {
            let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin_url = format!("http://{}", origin.local_addr().unwrap());
            let location = format!("http://{}/stolen", destination.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = origin.accept().await.unwrap();
                let mut request = [0; 8192];
                let _ = socket.read(&mut request).await.unwrap();
                socket.write_all(format!("HTTP/1.1 {status} Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            });
            let response = client()
                .get(origin_url)
                .header("cf-aig-authorization", "Bearer gateway-secret")
                .header("api-key", "azure-secret")
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), status);
            server.await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(20), destination.accept())
                    .await
                    .is_err()
            );
        }
    }
}
