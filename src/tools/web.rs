use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use std::net::IpAddr;
use url::Url;

use super::{Tool, require_str, truncate_for_context};

/// Maximum number of redirects to follow. Matches the previous
/// `redirect::Policy::limited(5)` behavior.
const MAX_REDIRECTS: u32 = 5;

pub struct FetchUrlTool {
    /// Local/internal ports the agent is permitted to reach despite the
    /// general SSRF block. Empty = original behavior (no internal access).
    allowed_local_ports: Vec<u16>,
}

impl FetchUrlTool {
    pub fn new(allowed_local_ports: Vec<u16>) -> Self {
        Self {
            allowed_local_ports,
        }
    }

    /// Validate that `url_str` is safe to fetch: scheme is http/https, the
    /// hostname isn't a known-internal name, and every DNS-resolved address
    /// for it is public (unless the destination port is allowlisted).
    ///
    /// This must be called for both the original URL and every redirect hop,
    /// and the returned address must be pinned for the actual connection
    /// (via `Client::resolve`) — otherwise reqwest re-resolves DNS
    /// independently, and a rebinding nameserver could pass validation here
    /// yet hand reqwest an internal address a moment later.
    async fn validate_target(&self, url_str: &str) -> Result<(Url, std::net::SocketAddr)> {
        // Only allow http/https
        if !url_str.starts_with("http://") && !url_str.starts_with("https://") {
            bail!("Only HTTP and HTTPS URLs are allowed");
        }

        let parsed = Url::parse(url_str)?;
        let host = parsed
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("URL has no host"))?;
        let port = parsed.port_or_known_default().unwrap_or(80);
        let port_allowlisted = self.allowed_local_ports.contains(&port);

        // Block known internal hostnames before DNS resolution.
        // Skip the block when the destination port is allowlisted.
        let lower_host = host.to_lowercase();
        let is_internal_hostname = lower_host == "localhost"
            || lower_host.ends_with(".local")
            || lower_host.ends_with(".internal");
        if is_internal_hostname && !port_allowlisted {
            bail!("Fetching internal/localhost URLs is not allowed");
        }

        // Resolve DNS and check all resulting IPs
        use tokio::net::lookup_host;
        let addrs: Vec<_> = lookup_host(format!("{}:{}", host, port))
            .await
            .map_err(|e| anyhow::anyhow!("DNS resolution failed for {}: {}", host, e))?
            .collect();

        if addrs.is_empty() {
            bail!("DNS resolution returned no addresses for {}", host);
        }

        for addr in &addrs {
            if is_internal_ip(addr.ip()) && !port_allowlisted {
                bail!(
                    "URL resolves to internal address {} — fetching internal URLs is not allowed",
                    addr.ip()
                );
            }
        }

        Ok((parsed, addrs[0]))
    }
}

/// Check if an IP address is private/loopback/link-local.
fn is_internal_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()          // 127.0.0.0/8
                || v4.is_private()    // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local() // 169.254/16
                || v4.is_unspecified() // 0.0.0.0
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()          // ::1
                || v6.is_unspecified() // ::
                // IPv4-mapped addresses (::ffff:127.0.0.1 etc.)
                || v6.to_ipv4_mapped().is_some_and(|v4| {
                    v4.is_loopback() || v4.is_private() || v4.is_link_local()
                })
        }
    }
}

#[async_trait]
impl Tool for FetchUrlTool {
    fn name(&self) -> &str {
        "fetch_url"
    }

    fn description_for_llm(&self) -> &str {
        "Fetch the content of a web page or API endpoint. \
         Parameters: {\"url\": \"<url>\"}. \
         Returns the response body as text. Useful for reading web pages, \
         APIs, documentation, RSS feeds, etc. Only HTTP and HTTPS URLs are allowed. \
         Internal/localhost addresses are blocked except for ports explicitly \
         allowed in config (e.g., the local wiki on 8088)."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let url_str = require_str(&params, "url")?;

        let (mut current_url, mut pinned_addr) = self.validate_target(url_str).await?;

        // Disable reqwest's built-in redirect following: it re-resolves DNS
        // independently and would follow a redirect straight to an internal
        // address without ever re-running our SSRF checks. Instead we follow
        // redirects manually, re-validating (scheme + DNS) on every hop and
        // pinning each connection to the address that passed validation.
        let response = {
            let mut hops = 0u32;
            loop {
                let host = current_url
                    .host_str()
                    .ok_or_else(|| anyhow::anyhow!("URL has no host"))?;
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .redirect(reqwest::redirect::Policy::none())
                    .resolve(host, pinned_addr)
                    .build()?;
                let resp = client.get(current_url.clone()).send().await?;
                if resp.status().is_redirection() {
                    hops += 1;
                    if hops > MAX_REDIRECTS {
                        bail!("Too many redirects (limit {})", MAX_REDIRECTS);
                    }
                    let location = resp
                        .headers()
                        .get(reqwest::header::LOCATION)
                        .ok_or_else(|| {
                            anyhow::anyhow!("Redirect response missing Location header")
                        })?
                        .to_str()
                        .context("Redirect Location header is not valid UTF-8")?;
                    let next_url = current_url
                        .join(location)
                        .with_context(|| format!("Invalid redirect Location: {}", location))?;
                    (current_url, pinned_addr) = self.validate_target(next_url.as_str()).await?;
                    continue;
                }
                break resp;
            }
        };
        let status = response.status();

        if !status.is_success() {
            bail!(
                "HTTP {}: {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("")
            );
        }

        let body = response.text().await?;

        // Truncate very large responses to avoid blowing up context
        const MAX_LEN: usize = 50_000;
        Ok(truncate_for_context(body, MAX_LEN, "response"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_internal_ip_loopback() {
        assert!(is_internal_ip("127.0.0.1".parse().unwrap()));
        assert!(is_internal_ip("127.255.255.255".parse().unwrap()));
        assert!(is_internal_ip("::1".parse().unwrap()));
    }

    #[test]
    fn test_is_internal_ip_rfc1918() {
        assert!(is_internal_ip("10.0.0.1".parse().unwrap()));
        assert!(is_internal_ip("172.16.0.1".parse().unwrap()));
        assert!(is_internal_ip("172.31.255.255".parse().unwrap()));
        assert!(is_internal_ip("192.168.1.1".parse().unwrap()));
        // IPv4-mapped IPv6 form of a private address.
        assert!(is_internal_ip("::ffff:10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn test_is_internal_ip_link_local() {
        assert!(is_internal_ip("169.254.1.1".parse().unwrap()));
        assert!(is_internal_ip("0.0.0.0".parse().unwrap()));
    }

    #[test]
    fn test_is_internal_ip_public() {
        assert!(!is_internal_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_internal_ip("1.1.1.1".parse().unwrap()));
        assert!(!is_internal_ip("93.184.216.34".parse().unwrap()));
        assert!(!is_internal_ip("2606:4700:4700::1111".parse().unwrap()));
    }
}
