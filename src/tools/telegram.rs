use anyhow::Result;
use async_trait::async_trait;

use super::Tool;

pub struct SendTelegramTool {
    bot_token: String,
    default_chat_id: Option<i64>,
}

impl SendTelegramTool {
    pub fn new(bot_token: String, default_chat_id: Option<i64>) -> Self {
        Self {
            bot_token,
            default_chat_id,
        }
    }
}

#[async_trait]
impl Tool for SendTelegramTool {
    fn name(&self) -> &str {
        "send_telegram"
    }

    fn description_for_llm(&self) -> &str {
        "Send a message via Telegram. \
         Parameters: {\"message\": \"<text>\", \"chat_id\": <optional number>}. \
         If chat_id is omitted, the message is sent to the default configured chat. \
         Use this to deliver results, notifications, or replies to the user via Telegram."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let message = params
            .get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: message"))?;

        let chat_id = params
            .get("chat_id")
            .and_then(|v| v.as_i64())
            .or(self.default_chat_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No chat_id provided and no default_chat_id configured in [telegram]"
                )
            })?;

        let base_url = format!("https://api.telegram.org/bot{}", self.bot_token);

        let client = reqwest::Client::new();

        // Telegram has a 4096 char limit per message
        let chunks = chunk_message(message, 4096);

        for chunk in &chunks {
            let resp = client
                .post(format!("{}/sendMessage", base_url))
                .json(&serde_json::json!({
                    "chat_id": chat_id,
                    "text": chunk,
                }))
                .send()
                .await?;

            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Telegram API error: {}", body);
            }
        }

        if chunks.len() == 1 {
            Ok("Message sent.".to_string())
        } else {
            Ok(format!("Message sent ({} parts).", chunks.len()))
        }
    }
}

fn chunk_message(text: &str, max_len: usize) -> Vec<&str> {
    if text.len() <= max_len {
        return vec![text];
    }

    let mut chunks = vec![];
    let mut start = 0;

    while start < text.len() {
        let mut end = (start + max_len).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end < text.len() {
            if let Some(newline_pos) = text[start..end].rfind('\n') {
                end = start + newline_pos + 1;
            }
        }
        chunks.push(&text[start..end]);
        start = end;
    }

    chunks
}
