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
}
