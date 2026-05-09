use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use super::{ChatMessage, LlmBackend};

pub struct LlamaCppBackend {
    client: Client,
    endpoint: String,
    model: Option<String>,
    slot: i32,
    /// Per-slot context window discovered from the server's `/props` endpoint.
    /// Cached after first successful fetch; never refreshed within a process.
    n_ctx: OnceCell<usize>,
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ApiMessage>,
    /// Pin to a specific llama.cpp slot for KV cache reuse across requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    id_slot: Option<i32>,
    /// Override the server's default n_predict. -1 = unlimited.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<i32>,
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
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: String,
    /// Gemma 4 (and other reasoning models) emit chain-of-thought tokens into
    /// this separate field when llama.cpp's chat-template parser is splitting
    /// them off `content`. Captured for observability only — not fed back into
    /// the agent loop.
    #[serde(default)]
    reasoning_content: Option<String>,
}

impl LlamaCppBackend {
    pub fn new(endpoint: String, model: Option<String>, slot: i32) -> Result<Self> {
        // Chat completions are non-streaming: llama-server is silent on the
        // wire for the entire generation, so a read_timeout would fire
        // mid-flight on legit long generations. Use a generous total timeout
        // instead — long enough that healthy responses finish, short enough
        // that a runaway/wedged generation eventually fails loudly. Revisit
        // once streaming is wired up (read_timeout becomes the right tool).
        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            client,
            endpoint,
            model,
            slot,
            n_ctx: OnceCell::new(),
        })
    }
}

#[derive(Deserialize)]
struct PropsResponse {
    /// Per-slot context size, e.g. 100096 for `-c 200000 -np 2`.
    default_generation_settings: PropsGeneration,
}

#[derive(Deserialize)]
struct PropsGeneration {
    n_ctx: usize,
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
            id_slot: Some(self.slot),
            max_tokens: Some(-1),
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

        let choice = chat_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No choices in llama.cpp response"))?;

        if let Some(reason) = &choice.finish_reason {
            if reason == "length" {
                tracing::warn!("LLM output truncated (finish_reason=length) — response may be incomplete");
            }
        }

        if let Some(reasoning) = &choice.message.reasoning_content
            && !reasoning.is_empty()
        {
            tracing::debug!(
                "LLM reasoning ({} chars): {}",
                reasoning.len(),
                reasoning
            );
        }

        Ok(choice.message.content)
    }

    async fn context_window_tokens(&self) -> Result<usize> {
        if let Some(&n) = self.n_ctx.get() {
            return Ok(n);
        }
        let url = format!("{}/props", self.endpoint.trim_end_matches('/'));
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch /props from llama-server")?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("llama.cpp /props returned HTTP {}", status);
        }
        let props: PropsResponse = resp
            .json()
            .await
            .context("Failed to parse /props response")?;
        let n = props.default_generation_settings.n_ctx;
        let _ = self.n_ctx.set(n);
        Ok(n)
    }
}
