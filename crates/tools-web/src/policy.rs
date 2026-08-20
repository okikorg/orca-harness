//! URL policy: the SSRF guard in front of every fetch.
//!
//! Checks the scheme (http/https only) and resolves the host, rejecting
//! anything that lands on a loopback, private, link-local, CGNAT, or
//! unique-local address — the ranges where cloud metadata endpoints and
//! internal services live. The vetted addresses are returned so the
//! caller can pin the connection to them, closing the classic
//! resolve-then-reconnect (DNS rebinding) gap.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use orca_harness_core::ToolError;
use reqwest::Url;

#[derive(Clone, Debug)]
pub struct UrlPolicy {
    allow_private: bool,
}

impl Default for UrlPolicy {
    fn default() -> Self {
        Self::strict()
    }
}

impl UrlPolicy {
    /// http/https only, public addresses only.
    pub fn strict() -> Self {
        Self {
            allow_private: false,
        }
    }

    /// Also allow loopback/private targets. For tests and deliberate
    /// localhost use (e.g. fetching from a dev server the agent started).
    pub fn allow_private(mut self) -> Self {
        self.allow_private = true;
        self
    }

    /// Validate `url` and return the vetted socket addresses to connect
    /// to. IP-literal hosts return that address; named hosts are resolved
    /// and every resolved address must pass.
    pub async fn check(&self, url: &Url) -> Result<Vec<SocketAddr>, ToolError> {
        match url.scheme() {
            "http" | "https" => {}
            other => {
                return Err(ToolError::msg(format!(
                    "unsupported scheme `{other}`; only http and https are allowed"
                )))
            }
        }
        let host = url
            .host_str()
            .ok_or_else(|| ToolError::msg("URL has no host"))?;
        let port = url.port_or_known_default().unwrap_or(443);

        let literal: Option<IpAddr> = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse()
            .ok();
        let addrs: Vec<SocketAddr> = match literal {
            Some(ip) => vec![SocketAddr::new(ip, port)],
            None => tokio::net::lookup_host((host, port))
                .await
                .map_err(|e| ToolError::msg(format!("DNS lookup for {host} failed: {e}")))?
                .collect(),
        };
        if addrs.is_empty() {
            return Err(ToolError::msg(format!("{host} did not resolve")));
        }
        if !self.allow_private {
            for addr in &addrs {
                if is_non_public(addr.ip()) {
                    return Err(ToolError::msg(format!(
                        "{host} resolves to a non-public address ({}); refusing",
                        addr.ip()
                    )));
                }
            }
        }
        Ok(addrs)
    }
}

fn is_non_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_non_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_non_public_v4(mapped);
            }
            v6.is_loopback() || v6.is_unspecified() || is_unique_local(v6) || is_v6_link_local(v6)
        }
    }
}

fn is_non_public_v4(v4: Ipv4Addr) -> bool {
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local() // includes 169.254.169.254 metadata endpoints
        || v4.is_unspecified()
        || v4.is_broadcast()
        || is_cgnat(v4)
}

/// 100.64.0.0/10 (RFC 6598 carrier-grade NAT, also used by tailnets).
fn is_cgnat(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 100 && (o[1] & 0xc0) == 64
}

/// fc00::/7
fn is_unique_local(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

/// fe80::/10
fn is_v6_link_local(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn check(policy: &UrlPolicy, url: &str) -> Result<Vec<SocketAddr>, ToolError> {
        policy.check(&Url::parse(url).unwrap()).await
    }

    #[tokio::test]
    async fn strict_rejects_non_public_literals() {
        let p = UrlPolicy::strict();
        for url in [
            "http://127.0.0.1/x",
            "http://10.1.2.3/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.100.1.1/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[fd00::1]/",
            "http://[::ffff:127.0.0.1]/",
        ] {
            assert!(check(&p, url).await.is_err(), "should reject {url}");
        }
    }

    #[tokio::test]
    async fn strict_accepts_public_literals_and_rejects_schemes() {
        let p = UrlPolicy::strict();
        assert!(check(&p, "https://93.184.216.34/").await.is_ok());
        assert!(check(&p, "ftp://93.184.216.34/").await.is_err());
        assert!(check(&p, "file:///etc/passwd").await.is_err());
    }

    #[tokio::test]
    async fn allow_private_admits_loopback() {
        let p = UrlPolicy::strict().allow_private();
        assert!(check(&p, "http://127.0.0.1:8080/").await.is_ok());
    }
}
