use anyhow::Result;
use teloxide::prelude::*;

use crate::client;
use crate::config::TelegramConfig;
use crate::protocol::{DaemonRequest, RequestKind};

pub async fn run(daemon_address: String, tg_config: TelegramConfig) -> Result<()> {
    let bot = Bot::new(&tg_config.bot_token);

    tracing::info!("Telegram bot starting...");

    let allowed_users = tg_config.allowed_users.clone();

    let handler = Update::filter_message().endpoint(
        move |bot: Bot, msg: Message, daemon_addr: String, allowed: Vec<u64>| async move {
            handle_message(bot, msg, &daemon_addr, &allowed).await
        },
    );

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![daemon_address, allowed_users])
        .default_handler(|_| async {})
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

async fn handle_message(
    bot: Bot,
    msg: Message,
    daemon_address: &str,
    allowed_users: &[u64],
) -> Result<(), teloxide::RequestError> {
    let text = match msg.text() {
        Some(t) => t,
        None => return Ok(()), // Ignore non-text messages
    };

    // Check user allowlist
    if !allowed_users.is_empty() {
        let user_id = msg.from.as_ref().map(|u| u.id.0).unwrap_or(0);
        if !allowed_users.contains(&user_id) {
            tracing::warn!("Telegram message from unauthorized user {}", user_id);
            return Ok(());
        }
    }

    tracing::info!("Telegram message from {:?}: {}", msg.from.as_ref().map(|u| u.id), text);

    // Handle /remember command
    let request = if let Some(fact) = text.strip_prefix("/remember ") {
        let fact = fact.trim();
        if fact.is_empty() {
            bot.send_message(msg.chat.id, "Usage: /remember <fact to remember>")
                .await?;
            return Ok(());
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

    let response = match client::send_request(daemon_address, &request).await {
        Ok(text) => text,
        Err(e) => format!("Error: {:#}", e),
    };

    // Telegram has a 4096 char message limit
    for chunk in chunk_message(&response, 4096) {
        bot.send_message(msg.chat.id, chunk).await?;
    }

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
