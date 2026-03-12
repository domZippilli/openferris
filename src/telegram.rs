use anyhow::{Context, Result};
use serde::Deserialize;

use openferris::config::TelegramConfig;
use openferris::protocol::{DaemonRequest, RequestKind};

use crate::client;

const TELEGRAM_API: &str = "https://api.telegram.org";

pub async fn run(daemon_address: String, tg_config: TelegramConfig) -> Result<()> {
    let http = reqwest::Client::new();
    let base_url = format!("{}/bot{}", TELEGRAM_API, tg_config.bot_token);

    tracing::info!("Telegram bot starting (long polling)...");

    let mut offset: i64 = 0;

    loop {
        let updates = match get_updates(&http, &base_url, offset).await {
            Ok(u) => u,
            Err(e) => {
                tracing::error!("Failed to get updates: {:#}", e);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        for update in updates {
            offset = update.update_id + 1;

            let message = match update.message {
                Some(m) => m,
                None => continue,
            };

            let text = match &message.text {
                Some(t) => t.clone(),
                None => continue,
            };

            let chat_id = message.chat.id;
            let user_id = message.from.as_ref().map(|u| u.id).unwrap_or(0);

            // Check user allowlist
            if !tg_config.allowed_users.is_empty() && !tg_config.allowed_users.contains(&user_id)
            {
                tracing::warn!("Telegram message from unauthorized user {}", user_id);
                continue;
            }

            tracing::info!("Telegram message from user {}", user_id);
            tracing::debug!("Telegram message content: {}", text);

            // Show "typing..." indicator while processing
            let _ = send_chat_action(&http, &base_url, chat_id, "typing").await;

            let response = handle_message(&text, &daemon_address).await;

            // Telegram has a 4096 char message limit
            for chunk in chunk_message(&response, 4096) {
                if let Err(e) = send_message(&http, &base_url, chat_id, chunk).await {
                    tracing::error!("Failed to send message: {:#}", e);
                }
            }
        }
    }
}

async fn handle_message(text: &str, daemon_address: &str) -> String {
    // Handle /remember command
    let request = if let Some(fact) = text.strip_prefix("/remember ") {
        let fact = fact.trim();
        if fact.is_empty() {
            return "Usage: /remember <fact to remember>".to_string();
        }
        DaemonRequest {
            id: uuid::Uuid::new_v4().to_string(),
            kind: RequestKind::StoreMemory {
                content: fact.to_string(),
            },
            source: Some("telegram".to_string()),
        }
    } else {
        DaemonRequest {
            id: uuid::Uuid::new_v4().to_string(),
            kind: RequestKind::FreeformMessage {
                text: text.to_string(),
            },
            source: Some("telegram".to_string()),
        }
    };

    match client::send_request(daemon_address, &request).await {
        Ok(text) => text,
        Err(e) => format!("Error: {:#}", e),
    }
}

// --- Telegram Bot API types (only what we need) ---

#[derive(Deserialize)]
struct TgResponse<T> {
    #[allow(dead_code)]
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct TgUpdate {
    update_id: i64,
    message: Option<TgMessage>,
}

#[derive(Deserialize)]
struct TgMessage {
    chat: TgChat,
    from: Option<TgUser>,
    text: Option<String>,
}

#[derive(Deserialize)]
struct TgChat {
    id: i64,
}

#[derive(Deserialize)]
struct TgUser {
    id: u64,
}

// --- Telegram Bot API calls ---

async fn get_updates(
    http: &reqwest::Client,
    base_url: &str,
    offset: i64,
) -> Result<Vec<TgUpdate>> {
    let resp: TgResponse<Vec<TgUpdate>> = http
        .get(format!("{}/getUpdates", base_url))
        .query(&[
            ("offset", offset.to_string()),
            ("timeout", "30".to_string()),
        ])
        .send()
        .await
        .context("Telegram API request failed")?
        .json()
        .await
        .context("Failed to parse Telegram response")?;

    resp.result
        .ok_or_else(|| anyhow::anyhow!("Telegram API error: {}", resp.description.unwrap_or_default()))
}

async fn send_chat_action(
    http: &reqwest::Client,
    base_url: &str,
    chat_id: i64,
    action: &str,
) -> Result<()> {
    http.post(format!("{}/sendChatAction", base_url))
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "action": action,
        }))
        .send()
        .await
        .context("Failed to send chat action")?;
    Ok(())
}

async fn send_message(
    http: &reqwest::Client,
    base_url: &str,
    chat_id: i64,
    text: &str,
) -> Result<()> {
    http.post(format!("{}/sendMessage", base_url))
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": text,
        }))
        .send()
        .await
        .context("Failed to send Telegram message")?;

    Ok(())
}

fn chunk_message(text: &str, max_len: usize) -> Vec<&str> {
    if text.len() <= max_len {
        return vec![text];
    }

    let mut chunks = vec![];
    let mut start = 0;

    while start < text.len() {
        let mut end = (start + max_len).min(text.len());
        // Don't split in the middle of a UTF-8 character
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        // Try to split at a newline for cleaner output
        if end < text.len()
            && let Some(newline_pos) = text[start..end].rfind('\n')
        {
            end = start + newline_pos + 1;
        }
        chunks.push(&text[start..end]);
        start = end;
    }

    chunks
}
