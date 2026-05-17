use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Deserialize;

use super::Tool;

const MAX_RESULTS: usize = 15;
const SNIPPET_CAP: usize = 200;

pub struct WebSearchTool {
    endpoint: String,
    client: reqwest::Client,
}

impl WebSearchTool {
    pub fn new(endpoint: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("reqwest client");
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            client,
        }
    }
}

#[derive(Deserialize)]
struct SearxResponse {
    #[serde(default)]
    results: Vec<SearxResult>,
}

#[derive(Deserialize)]
struct SearxResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description_for_llm(&self) -> &str {
        "Search the web via SearXNG metasearch. \
         Parameters: {\"query\": \"<search terms>\", \"categories\": \"<optional, default: general>\"}. \
         Returns a JSON array of {title, url, snippet}. Use this for discovery before fetch_url/scrape_url. \
         Capped at 15 results; snippets truncated to ~200 chars."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: query"))?;
        let categories = params
            .get("categories")
            .and_then(|v| v.as_str())
            .unwrap_or("general");

        let url = format!("{}/search", self.endpoint);
        let response = self
            .client
            .get(&url)
            .query(&[("q", query), ("format", "json"), ("categories", categories)])
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            bail!(
                "SearXNG returned HTTP {}: {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("")
            );
        }

        let parsed: SearxResponse = response.json().await?;
        let trimmed: Vec<serde_json::Value> = parsed
            .results
            .into_iter()
            .take(MAX_RESULTS)
            .map(|r| {
                let snippet = if r.content.len() > SNIPPET_CAP {
                    let mut end = SNIPPET_CAP;
                    while !r.content.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}…", &r.content[..end])
                } else {
                    r.content
                };
                serde_json::json!({
                    "title": r.title,
                    "url": r.url,
                    "snippet": snippet,
                })
            })
            .collect();

        Ok(serde_json::to_string_pretty(&trimmed)?)
    }
}
