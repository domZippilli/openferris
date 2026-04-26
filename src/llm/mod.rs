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

#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn chat_completion(&self, messages: &[ChatMessage]) -> anyhow::Result<String>;

    /// Per-slot context window size in tokens. Backends that talk to a server
    /// (llama.cpp) discover this at runtime; mocks and offline backends return
    /// a sensible constant. Used by the agent to decide when to compact.
    async fn context_window_tokens(&self) -> anyhow::Result<usize>;
}
