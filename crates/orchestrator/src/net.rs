//! Shared SSRF-safe HTTP fetching for server-initiated requests against
//! agent-controlled URLs (audit R3 finding 2). Fetchers must not follow
//! redirects blindly to arbitrary hosts (link-local/metadata endpoints) and
//! must reject targets that resolve to non-public addresses.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use reqwest::redirect::Policy;
use reqwest::Url;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
/// Bounded hop count for validated redirects.
pub const MAX_HOPS: usize = 3;
/// Reject oversized bodies before reading them.
const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct FetchPolicy {
    pub max_hops: usize,
    /// Test-only escape hatch that additionally permits loopback targets so
    /// integration tests can point the fetcher at a local server. Production
    /// callers must go through `fetch_public_text` (strict).
    pub allow_loopback: bool,
}

pub const STRICT: FetchPolicy = FetchPolicy {
    max_hops: MAX_HOPS,
    allow_loopback: false,
};

/// A resolved address is only usable when it is globally routable: loopback,
/// private ranges, link-local (cloud metadata), broadcast, documentation,
/// shared/CGNAT, reserved, multicast, and unspecified addresses are all
/// rejected. This is the single shared check for every fetcher (judge
/// extracts, artifacts probe).
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || o[0] == 0 // "this network" / unspecified
                || o[0] >= 224 // multicast (224/4) and reserved (240/4, broadcast)
                || (o[0] == 100 && (64..=127).contains(&o[1])) // shared/CGNAT
                || (o[0] == 192 && o[1] == 0 && o[2] == 0) // IETF protocol assignments
                || (o[0] == 192 && o[1] == 0 && o[2] == 2) // documentation
                || (o[0] == 198 && o[1] == 51 && o[2] == 100) // documentation
                || (o[0] == 203 && o[1] == 0 && o[2] == 113) // documentation
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19))) // benchmarking
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return false;
            }
            let s = v6.segments();
            // Unique-local (fc00::/7), link-local (fe80::/10), multicast.
            if (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 || (s[0] & 0xff00) == 0xff00 {
                return false;
            }
            // Documentation (2001:db8::/32).
            if s[0] == 0x2001 && s[1] == 0x0db8 {
                return false;
            }
            // IPv4-mapped (::ffff:0:0/96): apply the IPv4 rules to the
            // embedded address so mapped private/metadata addresses are
            // rejected too.
            if s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0xffff {
                let v4 = std::net::Ipv4Addr::new(
                    (s[6] >> 8) as u8,
                    s[6] as u8,
                    (s[7] >> 8) as u8,
                    s[7] as u8,
                );
                return is_public_ip(IpAddr::V4(v4));
            }
            // Global unicast is 2000::/3; be strict about anything else.
            (s[0] & 0xe000) == 0x2000
        }
    }
}

fn ip_allowed(ip: IpAddr, policy: FetchPolicy) -> bool {
    if is_public_ip(ip) {
        return true;
    }
    policy.allow_loopback && ip.is_loopback()
}

/// Strict fetch used by production code paths: no redirects followed without
/// validation, non-public resolved IPs rejected, credentials in the URL
/// rejected, bounded hops and body size.
pub async fn fetch_public_text(url: &str) -> Result<String, String> {
    fetch_text(url, STRICT).await
}

/// Fetch a URL as text under `policy`. Every hop (including the first) is
/// resolved and validated against the policy before a connection is made, and
/// the resolved addresses are pinned into the client so DNS cannot rebind
/// between validation and connect. Redirects are followed manually, one
/// validated hop at a time (`redirect(Policy::none())`).
pub async fn fetch_text(url: &str, policy: FetchPolicy) -> Result<String, String> {
    let mut current = Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
    for _ in 0..=policy.max_hops {
        validate_url(&current)?;
        let addrs = resolve_validated(&current, policy).await?;
        let client = client_for(&current, &addrs)?;
        let resp = client
            .get(current.clone())
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        if resp.status().is_redirection() {
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| "redirect without location".to_owned())?;
            current = current
                .join(loc)
                .map_err(|e| format!("bad redirect location: {e}"))?;
            validate_url(&current)?;
            continue;
        }
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("non-success status: {status}"));
        }
        if let Some(len) = resp.content_length() {
            if len > MAX_BODY_BYTES {
                return Err(format!("body too large: {len} bytes"));
            }
        }
        return resp
            .text()
            .await
            .map_err(|e| format!("body read failed: {e}"));
    }
    Err(format!("too many redirects (> {})", policy.max_hops))
}

fn validate_url(url: &Url) -> Result<(), String> {
    if url.username() != "" || url.password().is_some() {
        return Err("url credentials are not allowed".to_owned());
    }
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(format!("scheme {other:?} is not allowed")),
    }
    if url.host_str().is_none() {
        return Err("url has no host".to_owned());
    }
    Ok(())
}

/// Resolve the host and reject the URL unless every resolved address is
/// allowed by the policy.
async fn resolve_validated(url: &Url, policy: FetchPolicy) -> Result<Vec<SocketAddr>, String> {
    let host = url.host_str().expect("host checked").to_owned();
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => match tokio::net::lookup_host((host.as_str(), port)).await {
            Ok(iter) => iter.collect(),
            Err(e) => return Err(format!("dns resolution failed for {host}: {e}")),
        },
    };
    if addrs.is_empty() {
        return Err(format!("no addresses resolved for {host}"));
    }
    for addr in &addrs {
        if !ip_allowed(addr.ip(), policy) {
            return Err(format!(
                "target resolves to a non-public address: {}",
                addr.ip()
            ));
        }
    }
    Ok(addrs)
}

fn client_for(url: &Url, addrs: &[SocketAddr]) -> Result<reqwest::Client, String> {
    let host = url.host_str().expect("host checked").to_owned();
    let mut builder = reqwest::Client::builder()
        .timeout(DEFAULT_TIMEOUT)
        .redirect(Policy::none());
    builder = builder.resolve_to_addrs(&host, addrs);
    builder.build().map_err(|e| format!("client build: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn is_public_ip_rejects_non_routable_targets() {
        // loopback, RFC1918, link-local (cloud metadata), unspecified
        for ip in [
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(172, 16, 0, 5),
            Ipv4Addr::new(192, 168, 1, 1),
            Ipv4Addr::new(169, 254, 169, 254),
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::new(100, 64, 0, 1), // shared/CGNAT
            Ipv4Addr::new(192, 0, 2, 1),  // documentation
        ] {
            assert!(!is_public_ip(IpAddr::V4(ip)), "{ip} must be rejected");
        }
        assert!(is_public_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(is_public_ip(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        // IPv6: loopback, link-local, unique-local rejected; global accepted.
        assert!(!is_public_ip(IpAddr::V6("::1".parse().unwrap())));
        assert!(!is_public_ip(IpAddr::V6("fe80::1".parse().unwrap())));
        assert!(!is_public_ip(IpAddr::V6("fc00::1".parse().unwrap())));
        assert!(!is_public_ip(IpAddr::V6("::".parse().unwrap())));
        assert!(is_public_ip(IpAddr::V6("2606:4700::1111".parse().unwrap())));
    }

    #[tokio::test]
    async fn fetch_rejects_non_http_schemes_and_credentials() {
        assert!(fetch_public_text("ftp://example.com/x").await.is_err());
        assert!(fetch_public_text("http://user:pass@example.com/")
            .await
            .is_err());
    }
}
