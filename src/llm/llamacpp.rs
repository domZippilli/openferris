use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::{ChatMessage, LlmBackend};

pub struct LlamaCppBackend {
    client: Client,
    endpoint: String,
    model: Option<String>,
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ApiMessage>,
    /// Pin to a specific llama.cpp slot for KV cache reuse across requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    id_slot: Option<i32>,
}

#[derive(Serialize)]
struct ApiMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: String,
}

impl LlamaCppBackend {
    pub fn new(endpoint: String, model: Option<String>) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            client,
            endpoint,
            model,
        }
    }
}

#[async_trait]
impl LlmBackend for LlamaCppBackend {
    async fn chat_completion(&self, messages: &[ChatMessage]) -> Result<String> {
        let api_messages: Vec<ApiMessage> = messages
            .iter()
            .map(|m| ApiMessage {
                role: m.role.as_str().to_string(),
                content: m.content.clone(),
            })
            .collect();

        let request = ChatRequest {
            model: self.model.clone().unwrap_or_else(|| "default".to_string()),
            messages: api_messages,
            id_slot: Some(0),
        };

        let url = format!(
            "{}/v1/chat/completions",
            self.endpoint.trim_end_matches('/')
        );

        let response = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .context("Failed to connect to llama.cpp server")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("llama.cpp returned HTTP {}: {}", status, body);
        }

        let chat_response: ChatResponse = response
            .json()
            .await
            .context("Failed to parse llama.cpp response")?;

        chat_response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| anyhow::anyhow!("No choices in llama.cpp response"))
    }
}
