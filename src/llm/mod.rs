pub mod llamacpp;
pub mod mock;

use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone)]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub fn as_str(&self) -> &str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// Callback invoked for each text chunk as it streams in. Backends that
/// support real streaming call this many times per response; the default
/// (buffered) impl calls it exactly once with the full content.
pub type ChunkCallback<'a> = &'a mut (dyn FnMut(&str) + Send);

#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn chat_completion(&self, messages: &[ChatMessage]) -> anyhow::Result<String>;

    /// Streaming variant. Invokes `on_chunk` with text fragments as they
    /// arrive; returns the full accumulated content on success. Default impl
    /// buffers (calls `chat_completion` and emits one chunk) so backends can
    /// opt in to real streaming without breaking compilation.
    async fn chat_completion_stream(
        &self,
        messages: &[ChatMessage],
        on_chunk: ChunkCallback<'_>,
    ) -> anyhow::Result<String> {
        let full = self.chat_completion(messages).await?;
        on_chunk(&full);
        Ok(full)
    }

    /// Per-slot context window size in tokens. Backends that talk to a server
    /// (llama.cpp) discover this at runtime; mocks and offline backends return
    /// a sensible constant. Used by the agent to decide when to compact.
    async fn context_window_tokens(&self) -> anyhow::Result<usize>;
}
