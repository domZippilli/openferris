use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Deserialize;

use super::Tool;

const MAX_LEN: usize = 50_000;

pub struct ScrapeUrlTool {
    endpoint: String,
    client: reqwest::Client,
}

impl ScrapeUrlTool {
    pub fn new(endpoint: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(90))
            .build()
            .expect("reqwest client");
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            client,
        }
    }
}

#[derive(Deserialize)]
struct ScrapeResponse {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Option<ScrapeData>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct ScrapeData {
    #[serde(default)]
    markdown: Option<String>,
    #[serde(default)]
    metadata: serde_json::Value,
}

#[async_trait]
impl Tool for ScrapeUrlTool {
    fn name(&self) -> &str {
        "scrape_url"
    }

    fn description_for_llm(&self) -> &str {
        "Scrape a web page via Firecrawl and return clean LLM-ready markdown. \
         Parameters: {\"url\": \"<url>\"}. \
         Handles JavaScript-rendered pages, removes nav/chrome/ads. Returns markdown \
         (truncated at 50KB). Use this for general web pages where you want article content. \
         For simple/known endpoints (RSS, JSON APIs, your wiki) use fetch_url instead — \
         it's faster. For sites with aggressive bot detection, stealth_fetch is \
         the next escalation."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let url = params
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: url"))?;

        let body = serde_json::json!({
            "url": url,
            "formats": ["markdown"],
        });

        let response = self
            .client
            .post(format!("{}/v1/scrape", self.endpoint))
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            bail!("Firecrawl HTTP {}: {}", status.as_u16(), text);
        }

        let parsed: ScrapeResponse = response.json().await?;
        if !parsed.success {
            bail!(
                "Firecrawl returned success=false: {}",
                parsed.error.unwrap_or_else(|| "no error message".into())
            );
        }

        let data = parsed
            .data
            .ok_or_else(|| anyhow::anyhow!("Firecrawl response missing data"))?;
        let mut md = data.markdown.unwrap_or_default();

        let title = data
            .metadata
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let source_url = data
            .metadata
            .get("sourceURL")
            .and_then(|v| v.as_str())
            .unwrap_or(url);

        if md.len() > MAX_LEN {
            let mut end = MAX_LEN;
            while !md.is_char_boundary(end) {
                end -= 1;
            }
            let original = md.len();
            md.truncate(end);
            md.push_str(&format!(
                "\n\n[Truncated — markdown was {} bytes]",
                original
            ));
        }

        Ok(format!("# {}\n<{}>\n\n{}", title, source_url, md))
    }
}
