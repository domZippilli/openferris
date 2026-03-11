use anyhow::{bail, Result};
use async_trait::async_trait;

use super::Tool;

pub struct FetchUrlTool;

#[async_trait]
impl Tool for FetchUrlTool {
    fn name(&self) -> &str {
        "fetch_url"
    }

    fn description_for_llm(&self) -> &str {
        "Fetch the content of a web page or API endpoint. \
         Parameters: {\"url\": \"<url>\"}. \
         Returns the response body as text. Useful for reading web pages, \
         APIs, documentation, RSS feeds, etc. Only HTTP and HTTPS URLs are allowed."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let url = params
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: url"))?;

        // Only allow http/https
        if !url.starts_with("http://") && !url.starts_with("https://") {
            bail!("Only HTTP and HTTPS URLs are allowed");
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        let response = client.get(url).send().await?;
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
