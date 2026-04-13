use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use openferris::config::TelegramConfig;
use openferris::protocol::{DaemonRequest, DaemonResponse, RequestKind, ResponseKind};

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

            // Keep "typing..." indicator alive while the agent works
            let typing_http = http.clone();
            let typing_url = base_url.clone();
            let typing_handle = tokio::spawn(async move {
                loop {
                    let _ = send_chat_action(&typing_http, &typing_url, chat_id, "typing").await;
                    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                }
            });

            let response =
                handle_message(&text, &daemon_address, &http, &base_url, chat_id).await;
            typing_handle.abort();

            // Telegram has a 4096 char message limit
            for chunk in chunk_message(&response, 4096) {
                if let Err(e) = send_message(&http, &base_url, chat_id, chunk).await {
                    tracing::error!("Failed to send message: {:#}", e);
                }
            }
        }
    }
}

async fn handle_message(
    text: &str,
    daemon_address: &str,
    http: &reqwest::Client,
    base_url: &str,
    chat_id: i64,
) -> String {
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

    match send_request_with_progress(daemon_address, &request, http, base_url, chat_id).await {
        Ok(text) => text,
        Err(e) => format!("Error: {:#}", e),
    }
}

/// Send a daemon request and forward progress updates as Telegram status messages.
/// On first progress: sends a new message and records its ID.
/// On subsequent progress: edits the existing message.
/// On completion: deletes the status message and returns the final text.
async fn send_request_with_progress(
    socket_path: &str,
    request: &DaemonRequest,
    http: &reqwest::Client,
    base_url: &str,
    chat_id: i64,
) -> Result<String> {
    let stream = UnixStream::connect(socket_path)
        .await
        .context("Failed to connect to daemon")?;

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    let mut data = serde_json::to_string(request)?;
    data.push('\n');
    writer.write_all(data.as_bytes()).await?;

    let mut status_msg_id: Option<i64> = None;

    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        if line.is_empty() {
            anyhow::bail!("Daemon disconnected");
        }

        let response: DaemonResponse =
            serde_json::from_str(line.trim()).context("Failed to parse daemon response")?;

        match response.kind {
            ResponseKind::Done { text } => {
                // Clean up the status message
                if let Some(msg_id) = status_msg_id {
                    let _ = delete_message(http, base_url, chat_id, msg_id).await;
                }
                return Ok(text);
            }
            ResponseKind::Error { message } => {
                if let Some(msg_id) = status_msg_id {
                    let _ = delete_message(http, base_url, chat_id, msg_id).await;
                }
                anyhow::bail!("{}", message);
            }
            ResponseKind::Progress { text: label } => match status_msg_id {
                None => {
                    if let Ok(msg_id) =
                        send_message_get_id(http, base_url, chat_id, &label).await
                    {
                        status_msg_id = Some(msg_id);
                    }
                }
                Some(msg_id) => {
                    let _ = edit_message(http, base_url, chat_id, msg_id, &label).await;
                }
            },
        }
    }
}

// --- Telegram Bot API types (only what we need) ---

#[derive(Deserialize)]
struct TgResponse<T> {
    #[allow(dead_code)]
    ok: bool,
    result: Option<T>,
    #[allow(dead_code)]
    description: Option<String>,
}

#[derive(Deserialize)]
struct TgSentMessage {
    message_id: i64,
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

/// Send a Telegram message and return the message_id from the API response.
async fn send_message_get_id(
    http: &reqwest::Client,
    base_url: &str,
    chat_id: i64,
    text: &str,
) -> Result<i64> {
    let resp: TgResponse<TgSentMessage> = http
        .post(format!("{}/sendMessage", base_url))
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": text,
        }))
        .send()
        .await
        .context("Failed to send Telegram message")?
        .json()
        .await
        .context("Failed to parse sendMessage response")?;

    resp.result
        .map(|m| m.message_id)
        .ok_or_else(|| anyhow::anyhow!("sendMessage returned no result"))
}

async fn edit_message(
    http: &reqwest::Client,
    base_url: &str,
    chat_id: i64,
    message_id: i64,
    text: &str,
) -> Result<()> {
    http.post(format!("{}/editMessageText", base_url))
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
        }))
        .send()
        .await
        .context("Failed to edit Telegram message")?;
    Ok(())
}

async fn delete_message(
    http: &reqwest::Client,
    base_url: &str,
    chat_id: i64,
    message_id: i64,
) -> Result<()> {
    http.post(format!("{}/deleteMessage", base_url))
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
        }))
        .send()
        .await
        .context("Failed to delete Telegram message")?;
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
