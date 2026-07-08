use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Deserialize;

use super::{Tool, require_str, truncate_for_context};

const MAX_LEN: usize = 50_000;

pub struct StealthFetchTool {
    endpoint: String,
    client: reqwest::Client,
}

impl StealthFetchTool {
    pub fn new(endpoint: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .expect("reqwest client");
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            client,
        }
    }
}

#[derive(Deserialize)]
struct FetchResp {
    status: u16,
    url: String,
    #[serde(default)]
    markdown: String,
    #[serde(default)]
    html_bytes: u64,
}

#[async_trait]
impl Tool for StealthFetchTool {
    fn name(&self) -> &str {
        "stealth_fetch"
    }

    fn description_for_llm(&self) -> &str {
        "Fetch a web page through Camoufox (stealth Firefox with anti-fingerprinting) \
         and return clean markdown. Use this only when fetch_url and scrape_url are \
         blocked, rate-limited, or returning bot-detection pages. \
         Parameters: {\"url\": \"<url>\", \"wait_ms\": <optional int, 0-15000>}. \
         The wait_ms pauses after DOM load for JS-rendered content to populate. \
         Slow (~2-10s per call) and resource-heavy; reach for it last in the \
         fetch_url -> scrape_url -> stealth_fetch ladder."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let url = require_str(&params, "url")?;
        let wait_ms = params.get("wait_ms").and_then(|v| v.as_u64()).unwrap_or(0);

        let body = serde_json::json!({"url": url, "wait_ms": wait_ms});

        let response = self
            .client
            .post(format!("{}/fetch", self.endpoint))
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            bail!("Camoufox HTTP {}: {}", status.as_u16(), text);
        }

        let parsed: FetchResp = response.json().await?;

        let md = truncate_for_context(parsed.markdown, MAX_LEN, "markdown");

        Ok(format!(
            "<{}> (status {}, {} bytes raw HTML)\n\n{}",
            parsed.url, parsed.status, parsed.html_bytes, md
        ))
    }
}
