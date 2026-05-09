use std::collections::VecDeque;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;

use super::{ChatMessage, ChunkCallback, LlmBackend};

/// A mock LLM backend that returns pre-scripted responses in FIFO order.
/// Useful for deterministic testing of the agent loop.
pub struct MockLlm {
    responses: Mutex<VecDeque<String>>,
    call_log: Mutex<Vec<Vec<ChatMessage>>>,
    /// What `context_window_tokens` returns. Tests that exercise compaction
    /// pass a small value to force the threshold.
    n_ctx: usize,
}

impl MockLlm {
    pub fn new(responses: Vec<String>) -> Self {
        Self::with_n_ctx(responses, 1_000_000)
    }

    pub fn with_n_ctx(responses: Vec<String>, n_ctx: usize) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            call_log: Mutex::new(vec![]),
            n_ctx,
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

    /// Streams the next scripted response as whitespace-delimited chunks.
    /// Uses `split_inclusive(' ')` so trailing spaces are preserved on each
    /// chunk, meaning concatenating all chunks yields the original string
    /// byte-for-byte. Falls back to a single chunk for empty responses.
    async fn chat_completion_stream(
        &self,
        messages: &[ChatMessage],
        on_chunk: ChunkCallback<'_>,
    ) -> Result<String> {
        let full = self.chat_completion(messages).await?;
        let mut accumulated = String::with_capacity(full.len());
        if full.is_empty() {
            on_chunk(&full);
        } else {
            for piece in full.split_inclusive(' ') {
                on_chunk(piece);
                accumulated.push_str(piece);
            }
            debug_assert_eq!(accumulated, full);
        }
        Ok(full)
    }

    async fn context_window_tokens(&self) -> Result<usize> {
        Ok(self.n_ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn chat_completion_stream_emits_word_chunks() {
        let scripted = "hello world how are you".to_string();
        let mock = MockLlm::new(vec![scripted.clone()]);

        let mut chunks: Vec<String> = Vec::new();
        let mut cb = |chunk: &str| chunks.push(chunk.to_string());
        let returned = mock
            .chat_completion_stream(&[], &mut cb)
            .await
            .expect("stream should succeed");

        assert!(
            chunks.len() > 1,
            "expected multiple streamed chunks, got {}: {:?}",
            chunks.len(),
            chunks
        );
        assert_eq!(chunks.concat(), scripted);
        assert_eq!(returned, scripted);
    }
}
