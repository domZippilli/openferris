use std::collections::VecDeque;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;

use super::{ChatMessage, LlmBackend};

/// A mock LLM backend that returns pre-scripted responses in FIFO order.
/// Useful for deterministic testing of the agent loop.
pub struct MockLlm {
    responses: Mutex<VecDeque<String>>,
    call_log: Mutex<Vec<Vec<ChatMessage>>>,
}

impl MockLlm {
    pub fn new(responses: Vec<String>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            call_log: Mutex::new(vec![]),
        }
    }

    /// How many times `chat_completion` has been called.
    pub fn call_count(&self) -> usize {
        self.call_log.lock().unwrap().len()
    }

    /// The messages sent in the Nth call (0-indexed).
    pub fn messages_at(&self, index: usize) -> Option<Vec<ChatMessage>> {
        self.call_log.lock().unwrap().get(index).cloned()
    }
}

#[async_trait]
impl LlmBackend for MockLlm {
    async fn chat_completion(&self, messages: &[ChatMessage]) -> Result<String> {
        self.call_log.lock().unwrap().push(messages.to_vec());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("MockLlm: no more scripted responses"))
    }
}
