use anyhow::{bail, Result};
use async_trait::async_trait;
use std::net::IpAddr;
use url::Url;

use super::Tool;

pub struct FetchUrlTool;

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
         Cannot fetch localhost or internal network addresses."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let url_str = params
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: url"))?;

        // Only allow http/https
        if !url_str.starts_with("http://") && !url_str.starts_with("https://") {
            bail!("Only HTTP and HTTPS URLs are allowed");
        }

        let parsed = Url::parse(url_str)?;
        let host = parsed
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("URL has no host"))?;

        // Block known internal hostnames before DNS resolution
        let lower_host = host.to_lowercase();
        if lower_host == "localhost"
            || lower_host.ends_with(".local")
            || lower_host.ends_with(".internal")
        {
            bail!("Fetching internal/localhost URLs is not allowed");
        }

        // Resolve DNS and check all resulting IPs
        use tokio::net::lookup_host;
        let port = parsed.port_or_known_default().unwrap_or(80);
        let addrs: Vec<_> = lookup_host(format!("{}:{}", host, port))
            .await
            .map_err(|e| anyhow::anyhow!("DNS resolution failed for {}: {}", host, e))?
            .collect();

        if addrs.is_empty() {
            bail!("DNS resolution returned no addresses for {}", host);
        }

        for addr in &addrs {
            if is_internal_ip(addr.ip()) {
                bail!(
                    "URL resolves to internal address {} — fetching internal URLs is not allowed",
                    addr.ip()
                );
            }
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()?;

        let response = client.get(url_str).send().await?;
        let status = response.status();

        if !status.is_success() {
            bail!("HTTP {}: {}", status.as_u16(), status.canonical_reason().unwrap_or(""));
        }

        let body = response.text().await?;

        // Truncate very large responses to avoid blowing up context
        const MAX_LEN: usize = 50_000;
        if body.len() > MAX_LEN {
            let mut end = MAX_LEN;
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            Ok(format!("{}\n\n[Truncated — response was {} bytes]", &body[..end], body.len()))
        } else {
            Ok(body)
        }
    }
}
