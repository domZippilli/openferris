use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use super::{ChatMessage, ChunkCallback, LlmBackend};

/// Linear search for `needle` in `haystack`. Used to find the SSE message
/// delimiter (`\n\n`) inside a raw byte buffer. Buffer sizes here are small
/// (a few KB at most between drains), so a naive scan is fine.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

pub struct OpenAiCompatBackend {
    client: Client,
    endpoint: String,
    model: Option<String>,
    temperature: f32,
    top_k: u32,
    enable_thinking: bool,
    slot: i32,
    /// Per-slot context window discovered from the server's `/props` endpoint.
    /// Cached after first successful fetch; never refreshed within a process.
    n_ctx: OnceCell<usize>,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: String,
    messages: Vec<ApiMessage<'a>>,
    /// Pin to a specific llama.cpp slot for KV cache reuse across requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    id_slot: Option<i32>,
    /// Override the server's default token cap. `None` omits the field, which
    /// both llama.cpp and vLLM treat as "generate to EOS" (vLLM rejects -1).
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<i32>,
    temperature: f32,
    top_k: u32,
    /// Server-Sent Events streaming of generated tokens. Only set when the
    /// caller wants per-chunk delivery; non-streaming callers omit it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_template_kwargs: Option<ChatTemplateKwargs>,
}

#[derive(Clone, Serialize)]
struct ChatTemplateKwargs {
    enable_thinking: bool,
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
}

/// Borrows straight from the caller's `&[ChatMessage]` instead of cloning
/// every message's full content on every request — serde serializes borrowed
/// `&str` fields the same as owned `String`s, so this is a pure allocation
/// saving with no wire-format change.
#[derive(Serialize)]
struct ApiMessage<'a> {
    role: &'a str,
    content: &'a str,
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
    /// these separate fields when the backend's chat-template parser splits
    /// them off `content`. Captured for observability only — not fed back into
    /// the agent loop.
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
}

impl OpenAiCompatBackend {
    pub fn new(
        endpoint: String,
        model: Option<String>,
        temperature: f32,
        top_k: u32,
        enable_thinking: bool,
        slot: i32,
    ) -> Result<Self> {
        // Use a total `timeout` rather than a `read_timeout`: even in the
        // streaming path, the server can go silent on the wire for a while
        // between SSE chunks during legitimate long generations (thinking,
        // long tool-free completions), so a short read_timeout would fire
        // mid-flight. A generous total timeout is the tool that fits both
        // paths — long enough that healthy responses (streamed or not)
        // finish, short enough that a runaway/wedged generation eventually
        // fails loudly instead of hanging the caller forever.
        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            client,
            endpoint,
            model,
            temperature,
            top_k,
            enable_thinking,
            slot,
            n_ctx: OnceCell::new(),
        })
    }

    fn chat_template_kwargs(&self) -> Option<ChatTemplateKwargs> {
        self.enable_thinking.then_some(ChatTemplateKwargs {
            enable_thinking: true,
        })
    }

    /// Build the shared `ChatRequest` body for both the blocking and
    /// streaming completion paths; `stream` is the only field that differs
    /// between callers. Borrows message content straight from `messages`
    /// rather than cloning it.
    fn build_request<'a>(&self, messages: &'a [ChatMessage], stream: bool) -> ChatRequest<'a> {
        let api_messages: Vec<ApiMessage<'a>> = messages
            .iter()
            .map(|m| ApiMessage {
                role: m.role.as_str(),
                content: m.content.as_str(),
            })
            .collect();

        ChatRequest {
            model: self.model.clone().unwrap_or_else(|| "default".to_string()),
            messages: api_messages,
            id_slot: Some(self.slot),
            max_tokens: None,
            temperature: self.temperature,
            top_k: self.top_k,
            stream,
            chat_template_kwargs: self.chat_template_kwargs(),
        }
    }

    /// POST a built `ChatRequest` to the chat completions endpoint and return
    /// the raw (status-checked) `reqwest::Response`. A non-2xx status becomes
    /// an `anyhow::Error` with the response body attached. Stops short of
    /// consuming the body since the blocking and streaming callers read it
    /// differently (whole-body JSON parse vs an SSE byte stream).
    async fn post_chat(&self, request: &ChatRequest<'_>) -> Result<reqwest::Response> {
        let url = format!(
            "{}/v1/chat/completions",
            self.endpoint.trim_end_matches('/')
        );

        let mut req = self.client.post(&url).json(request);
        if request.stream {
            req = req.header("Accept", "text/event-stream");
        }

        let response = req
            .send()
            .await
            .context("Failed to connect to OpenAI-compatible chat server")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "OpenAI-compatible chat server returned HTTP {}: {}",
                status,
                body
            );
        }

        Ok(response)
    }
}

/// Warn when a completion's `finish_reason` indicates the server truncated
/// the output at its token cap rather than the model choosing to stop —
/// shared by the blocking and streaming completion paths.
fn warn_if_truncated(reason: Option<&str>) {
    if reason == Some("length") {
        tracing::warn!("LLM output truncated (finish_reason=length) — response may be incomplete");
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

/// vLLM's OpenAI `/v1/models` reports the context window as `max_model_len`
/// per model; used as a fallback when `/props` (llama.cpp-only) is absent.
#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    max_model_len: Option<usize>,
}

#[async_trait]
impl LlmBackend for OpenAiCompatBackend {
    async fn chat_completion(&self, messages: &[ChatMessage]) -> Result<String> {
        let request = self.build_request(messages, false);
        let response = self.post_chat(&request).await?;

        let chat_response: ChatResponse = response
            .json()
            .await
            .context("Failed to parse OpenAI-compatible chat response")?;

        let choice = chat_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No choices in OpenAI-compatible chat response"))?;

        warn_if_truncated(choice.finish_reason.as_deref());

        if let Some(reasoning) = choice
            .message
            .reasoning
            .as_ref()
            .or(choice.message.reasoning_content.as_ref())
            && !reasoning.is_empty()
        {
            tracing::debug!("LLM reasoning ({} chars): {}", reasoning.len(), reasoning);
        }

        Ok(choice.message.content)
    }

    async fn chat_completion_stream(
        &self,
        messages: &[ChatMessage],
        on_chunk: ChunkCallback<'_>,
    ) -> Result<String> {
        let request = self.build_request(messages, true);
        let response = self.post_chat(&request).await?;

        let mut stream = response.bytes_stream();
        // SSE messages are separated by a blank line ("\n\n"). A single TCP
        // chunk may contain multiple messages, a fragment of one, or a split
        // straddling the delimiter (or even mid-UTF-8-codepoint) — so we
        // accumulate raw bytes here and peel off complete messages one at
        // a time, only attempting UTF-8 decode on whole messages.
        let mut buffer: Vec<u8> = Vec::new();
        let mut accumulated = String::new();
        let mut reasoning_accumulated = String::new();
        let mut final_finish_reason: Option<String> = None;
        let mut done = false;

        while let Some(chunk) = stream.next().await {
            let bytes = chunk
                .context("Error reading streaming response from OpenAI-compatible chat server")?;
            buffer.extend_from_slice(&bytes);

            while let Some(idx) = find_subslice(&buffer, b"\n\n") {
                let raw: Vec<u8> = buffer.drain(..idx + 2).collect();
                let message_bytes = &raw[..raw.len() - 2];
                let message = std::str::from_utf8(message_bytes)
                    .context("OpenAI-compatible chat SSE message was not valid UTF-8")?;

                // An SSE message is one or more lines; each `data:` line
                // contributes one logical payload. llama.cpp packs the JSON
                // onto a single line, but be permissive in case of folding.
                let mut payload = String::new();
                for line in message.lines() {
                    if let Some(rest) = line.strip_prefix("data:") {
                        if !payload.is_empty() {
                            payload.push('\n');
                        }
                        payload.push_str(rest.strip_prefix(' ').unwrap_or(rest));
                    }
                    // Other SSE fields (event:, id:, retry:, comments) are ignored.
                }

                if payload.is_empty() {
                    continue;
                }
                if payload == "[DONE]" {
                    done = true;
                    break;
                }

                let parsed: StreamChunk = match serde_json::from_str(&payload) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("Failed to parse SSE chunk: {} (payload: {})", e, payload);
                        continue;
                    }
                };

                if let Some(choice) = parsed.choices.into_iter().next() {
                    if let Some(reason) = choice.finish_reason {
                        final_finish_reason = Some(reason);
                    }
                    if let Some(reasoning) =
                        choice.delta.reasoning.or(choice.delta.reasoning_content)
                        && !reasoning.is_empty()
                    {
                        reasoning_accumulated.push_str(&reasoning);
                    }
                    if let Some(content) = choice.delta.content
                        && !content.is_empty()
                    {
                        on_chunk(&content);
                        accumulated.push_str(&content);
                    }
                }
            }

            if done {
                break;
            }
        }

        warn_if_truncated(final_finish_reason.as_deref());
        if !reasoning_accumulated.is_empty() {
            tracing::debug!(
                "LLM reasoning ({} chars): {}",
                reasoning_accumulated.len(),
                reasoning_accumulated
            );
        }

        Ok(accumulated)
    }

    async fn context_window_tokens(&self) -> Result<usize> {
        if let Some(&n) = self.n_ctx.get() {
            return Ok(n);
        }
        let base = self.endpoint.trim_end_matches('/');

        // llama.cpp exposes the per-slot context via /props. vLLM has no /props
        // but reports max_model_len on the OpenAI /v1/models route, so fall back
        // to that — keeping one client compatible with either backend.
        let props = async {
            let resp = self.client.get(format!("{base}/props")).send().await?;
            if !resp.status().is_success() {
                anyhow::bail!("/props returned HTTP {}", resp.status());
            }
            let p: PropsResponse = resp.json().await?;
            anyhow::Ok(p.default_generation_settings.n_ctx)
        }
        .await;

        let n = match props {
            Ok(n) => n,
            Err(props_err) => {
                let resp = self
                    .client
                    .get(format!("{base}/v1/models"))
                    .send()
                    .await
                    .context("Neither /props nor /v1/models reachable")?;
                if !resp.status().is_success() {
                    anyhow::bail!(
                        "context window unavailable: /props failed ({props_err:#}) \
                         and /v1/models returned HTTP {}",
                        resp.status()
                    );
                }
                let models: ModelsResponse = resp
                    .json()
                    .await
                    .context("Failed to parse /v1/models response")?;
                models
                    .data
                    .iter()
                    .find_map(|m| m.max_model_len)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "/v1/models reported no max_model_len (/props: {props_err:#})"
                        )
                    })?
            }
        };
        let _ = self.n_ctx.set(n);
        Ok(n)
    }
}
