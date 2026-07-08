use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use openferris::config::TelegramConfig;
use openferris::protocol::{DaemonRequest, DaemonResponse, RequestKind, ResponseKind};

const TELEGRAM_API: &str = "https://api.telegram.org";

pub async fn run(daemon_address: String, tg_config: TelegramConfig) -> Result<()> {
    // `getUpdates` long-polls for up to 30s (see the `timeout` query param
    // below). The client timeout must exceed that or we'll tear down a
    // perfectly healthy long-poll connection every cycle; it also bounds how
    // long a half-open connection can hang the bot loop.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .build()
        .context("Failed to build Telegram HTTP client")?;
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
            if !tg_config.allowed_users.is_empty() && !tg_config.allowed_users.contains(&user_id) {
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
                    if let Err(e) =
                        send_chat_action(&typing_http, &typing_url, chat_id, "typing").await
                    {
                        tracing::debug!("Failed to send typing indicator: {:#}", e);
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                }
            });

            let response = handle_message(&text, &daemon_address, &http, &base_url, chat_id).await;
            typing_handle.abort();

            // If streaming already rendered the final reply via live edits to
            // a dedicated message, `handle_message` returns an empty string —
            // skip sending another copy. Otherwise, fall back to the buffered
            // send path (e.g. backend didn't stream, or response was empty
            // assistant text wrapping a tool-call only flow).
            if response.is_empty() {
                continue;
            }

            // Telegram has a 4096 char message limit
            for chunk in chunk_message(&response, 4096) {
                if let Err(e) = send_message(&http, &base_url, chat_id, &chunk).await {
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
    // Handle slash commands.
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
            session_id: None,
        }
    } else if let Some(args) = text.strip_prefix("/goal ") {
        let (max_turns, exit_criteria) = match parse_goal_args(args) {
            Ok(parsed) => parsed,
            Err(e) => return e,
        };
        DaemonRequest {
            id: uuid::Uuid::new_v4().to_string(),
            kind: RequestKind::PursueGoal {
                exit_criteria,
                max_turns,
            },
            source: Some("telegram".to_string()),
            session_id: None,
        }
    } else {
        DaemonRequest {
            id: uuid::Uuid::new_v4().to_string(),
            kind: RequestKind::FreeformMessage {
                text: text.to_string(),
            },
            source: Some("telegram".to_string()),
            // Thread by chat: every message in this chat shares history,
            // even though each opens a fresh daemon connection.
            session_id: Some(format!("telegram:{}", chat_id)),
        }
    };

    match send_request_with_progress(daemon_address, &request, http, base_url, chat_id).await {
        Ok(text) => text,
        Err(e) => format!("Error: {:#}", e),
    }
}

fn parse_goal_args(input: &str) -> Result<(usize, String), String> {
    let mut parts = input.split_whitespace().peekable();
    let mut max_turns = 5usize;
    let mut criteria = Vec::new();

    while let Some(part) = parts.next() {
        if part == "--max-turns" {
            let Some(raw) = parts.next() else {
                return Err("Usage: /goal [--max-turns N] <exit criteria>".to_string());
            };
            max_turns = raw
                .parse::<usize>()
                .map_err(|_| "max turns must be a positive integer".to_string())?;
        } else if let Some(raw) = part.strip_prefix("--max-turns=") {
            max_turns = raw
                .parse::<usize>()
                .map_err(|_| "max turns must be a positive integer".to_string())?;
        } else {
            criteria.push(part);
            criteria.extend(parts);
            break;
        }
    }

    let exit_criteria = criteria.join(" ").trim().to_string();
    if exit_criteria.is_empty() {
        return Err("Usage: /goal [--max-turns N] <exit criteria>".to_string());
    }

    Ok((max_turns, exit_criteria))
}

/// Telegram messages are capped at 4096 chars. We leave a little headroom for
/// the truncation marker.
const TG_MSG_LIMIT: usize = 4096;
/// Minimum gap between successive `editMessageText` calls for the streaming
/// message. Telegram tolerates ~1 edit/sec; 1.5s is a safe ceiling that keeps
/// the UX feeling live without tripping flood limits on long replies.
const STREAM_EDIT_DEBOUNCE_MS: u128 = 1500;

/// Truncate `s` to fit in a single Telegram message, appending an ellipsis if
/// we had to cut. Splits at a char boundary so we never produce invalid UTF-8.
fn truncate_for_telegram(s: &str) -> String {
    if s.len() <= TG_MSG_LIMIT {
        return s.to_string();
    }
    const MARKER: &str = "\n\n[truncated]";
    let budget = TG_MSG_LIMIT.saturating_sub(MARKER.len());
    let mut end = budget.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + MARKER.len());
    out.push_str(&s[..end]);
    out.push_str(MARKER);
    out
}

/// Send a daemon request and forward progress + streamed chunks to Telegram.
///
/// Two distinct messages are managed in parallel:
///   * `status_msg_id` — the "Working..." / "Reading a file..." status line,
///     edited on each `Progress` and deleted on `Done`/`Error`.
///   * `chunk_msg_id`  — the live-streaming assistant prose message, sent on
///     the first `AssistantChunk`, edited (debounced) thereafter, and given a
///     final edit on `Done`. This message is the user's final reply.
///
/// Returns the final assistant text only when no streaming message was
/// rendered (caller sends it as a fresh message). When streaming did render
/// the reply in place, returns an empty string and the caller skips the send.
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
    let mut chunk_msg_id: Option<i64> = None;
    let mut chunk_buffer = String::new();
    let mut last_edit: Option<std::time::Instant> = None;

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
                // Clean up the status message regardless.
                if let Some(msg_id) = status_msg_id
                    && let Err(e) = delete_message(http, base_url, chat_id, msg_id).await
                {
                    tracing::debug!("Failed to delete status message: {:#}", e);
                }

                if let Some(msg_id) = chunk_msg_id {
                    // We already rendered (most of) the reply via streaming.
                    // Do a final edit so the message reflects the canonical
                    // final text (which may differ slightly from the buffered
                    // chunks — e.g. tool-call markup stripped, trailing
                    // whitespace cleaned). Only edit when there's something
                    // to show; if `text` is empty, leave the streamed buffer
                    // in place rather than blanking the message.
                    if !text.is_empty() {
                        let final_text = truncate_for_telegram(&text);
                        if let Err(e) =
                            edit_message(http, base_url, chat_id, msg_id, &final_text).await
                        {
                            tracing::warn!("Failed to send final streamed edit: {:#}", e);
                        }
                    }
                    // Sentinel: caller should NOT send `text` as a new
                    // message — the streamed message is the reply.
                    return Ok(String::new());
                }

                return Ok(text);
            }
            ResponseKind::Error { message } => {
                if let Some(msg_id) = status_msg_id
                    && let Err(e) = delete_message(http, base_url, chat_id, msg_id).await
                {
                    tracing::debug!("Failed to delete status message: {:#}", e);
                }
                // Clean up any half-rendered streaming message so the user
                // isn't left looking at a partial reply when we surface the
                // error.
                if let Some(msg_id) = chunk_msg_id
                    && let Err(e) = delete_message(http, base_url, chat_id, msg_id).await
                {
                    tracing::debug!("Failed to delete streaming message: {:#}", e);
                }
                anyhow::bail!("{}", message);
            }
            ResponseKind::Progress { text: label } => match status_msg_id {
                None => {
                    if let Ok(msg_id) = send_message_get_id(http, base_url, chat_id, &label).await {
                        status_msg_id = Some(msg_id);
                    }
                }
                Some(msg_id) => {
                    if let Err(e) = edit_message(http, base_url, chat_id, msg_id, &label).await {
                        tracing::debug!("Failed to edit progress message: {:#}", e);
                    }
                }
            },
            ResponseKind::AssistantChunk { text } => {
                chunk_buffer.push_str(&text);

                match chunk_msg_id {
                    None => {
                        // First chunk: send a new message we'll edit going
                        // forward. If the send fails (rate limit, network),
                        // drop the chunk silently — `Done` will still produce
                        // a fallback buffered send.
                        let initial = truncate_for_telegram(&chunk_buffer);
                        if !initial.is_empty()
                            && let Ok(msg_id) =
                                send_message_get_id(http, base_url, chat_id, &initial).await
                        {
                            chunk_msg_id = Some(msg_id);
                            last_edit = Some(std::time::Instant::now());
                        }
                    }
                    Some(msg_id) => {
                        // Debounce: only edit when enough time has passed
                        // since the previous edit. This bounds our edit rate
                        // to ~1 every 1.5s regardless of how fast chunks
                        // arrive.
                        let due = match last_edit {
                            None => true,
                            Some(t) => t.elapsed().as_millis() >= STREAM_EDIT_DEBOUNCE_MS,
                        };
                        if due {
                            let body = truncate_for_telegram(&chunk_buffer);
                            if let Err(e) =
                                edit_message(http, base_url, chat_id, msg_id, &body).await
                            {
                                tracing::debug!("Failed to edit streamed message: {:#}", e);
                            }
                            last_edit = Some(std::time::Instant::now());
                        }
                    }
                }
            }
        }
    }
}

// --- Telegram Bot API types (only what we need) ---

#[derive(Deserialize)]
struct TgResponse<T> {
    ok: bool,
    result: Option<T>,
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

async fn get_updates(http: &reqwest::Client, base_url: &str, offset: i64) -> Result<Vec<TgUpdate>> {
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

    resp.result.ok_or_else(|| {
        anyhow::anyhow!(
            "Telegram API error: {}",
            resp.description.unwrap_or_default()
        )
    })
}

/// Check that a Telegram API HTTP response actually succeeded, both at the
/// transport level (HTTP status) and the application level (the response
/// body's `ok` field). Telegram returns a JSON body with `ok: false` and a
/// human-readable `description` even on 4xx/5xx responses, so surface that
/// description in the error when available.
async fn check_tg_response(resp: reqwest::Response, method: &str) -> Result<()> {
    let status = resp.status();
    let body = resp
        .text()
        .await
        .with_context(|| format!("Failed to read {} response body", method))?;

    let parsed: Option<TgResponse<serde_json::Value>> = serde_json::from_str(&body).ok();
    let ok = parsed.as_ref().is_some_and(|r| r.ok);

    if status.is_success() && ok {
        return Ok(());
    }

    let description = parsed.and_then(|r| r.description).unwrap_or(body);

    anyhow::bail!("Telegram {} failed ({}): {}", method, status, description);
}

async fn send_chat_action(
    http: &reqwest::Client,
    base_url: &str,
    chat_id: i64,
    action: &str,
) -> Result<()> {
    let resp = http
        .post(format!("{}/sendChatAction", base_url))
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "action": action,
        }))
        .send()
        .await
        .context("Failed to send chat action")?;

    check_tg_response(resp, "sendChatAction").await
}

async fn send_message(
    http: &reqwest::Client,
    base_url: &str,
    chat_id: i64,
    text: &str,
) -> Result<()> {
    let resp = http
        .post(format!("{}/sendMessage", base_url))
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": text,
        }))
        .send()
        .await
        .context("Failed to send Telegram message")?;

    check_tg_response(resp, "sendMessage").await
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
    let resp = http
        .post(format!("{}/editMessageText", base_url))
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
        }))
        .send()
        .await
        .context("Failed to edit Telegram message")?;

    check_tg_response(resp, "editMessageText").await
}

async fn delete_message(
    http: &reqwest::Client,
    base_url: &str,
    chat_id: i64,
    message_id: i64,
) -> Result<()> {
    let resp = http
        .post(format!("{}/deleteMessage", base_url))
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
        }))
        .send()
        .await
        .context("Failed to delete Telegram message")?;

    check_tg_response(resp, "deleteMessage").await
}

/// A Telegram-HTML tag emitted by this file's HTML conversion. These tags
/// never nest (each span's inner text is copied verbatim rather than
/// re-processed for further formatting), so at most one such span is ever
/// open at any position in the text.
struct TagOccurrence {
    /// Byte offset of the first character of the tag markup (the `<`).
    start: usize,
    /// Byte offset just past the tag markup.
    end: usize,
    open: bool,
    /// For an opening tag, the exact markup to use when reopening it in a
    /// later chunk (e.g. `<a href="...">`, including the URL). Empty for
    /// closing tags.
    open_text: String,
    /// The matching closing tag, e.g. `</b>`.
    close_text: String,
}

/// Longest closing tag this file's HTML conversion ever emits. Used to
/// reserve enough room for a span that opens near a chunk boundary and
/// isn't closed before we run out of budget.
const MAX_CLOSE_TAG_LEN: usize = "</code>".len();

/// Scan `text` for the small set of HTML tags Telegram accepts that this
/// file's HTML conversion ever emits (`<b>`, `<i>`, `<code>`, `<pre>`,
/// `<a href="...">`), in order of appearance.
fn scan_html_tags(text: &str) -> Vec<TagOccurrence> {
    const SIMPLE_TAGS: &[(&str, &str)] = &[
        ("<b>", "</b>"),
        ("<i>", "</i>"),
        ("<code>", "</code>"),
        ("<pre>", "</pre>"),
    ];

    let mut occurrences = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if text.as_bytes()[i] == b'<' {
            let rest = &text[i..];

            if let Some((open, close)) = SIMPLE_TAGS.iter().find(|(open, _)| rest.starts_with(open))
            {
                occurrences.push(TagOccurrence {
                    start: i,
                    end: i + open.len(),
                    open: true,
                    open_text: (*open).to_string(),
                    close_text: (*close).to_string(),
                });
                i += open.len();
                continue;
            }
            if let Some((_, close)) = SIMPLE_TAGS
                .iter()
                .find(|(_, close)| rest.starts_with(close))
            {
                occurrences.push(TagOccurrence {
                    start: i,
                    end: i + close.len(),
                    open: false,
                    open_text: String::new(),
                    close_text: (*close).to_string(),
                });
                i += close.len();
                continue;
            }
            if rest.starts_with("</a>") {
                occurrences.push(TagOccurrence {
                    start: i,
                    end: i + 4,
                    open: false,
                    open_text: String::new(),
                    close_text: "</a>".to_string(),
                });
                i += 4;
                continue;
            }
            if rest.starts_with("<a href=\"")
                && let Some(rel_close) = rest.find("\">")
            {
                let tag_len = rel_close + 2;
                occurrences.push(TagOccurrence {
                    start: i,
                    end: i + tag_len,
                    open: true,
                    open_text: rest[..tag_len].to_string(),
                    close_text: "</a>".to_string(),
                });
                i += tag_len;
                continue;
            }
        }

        i += 1;
        while i < text.len() && !text.is_char_boundary(i) {
            i += 1;
        }
    }
    occurrences
}

/// Split `text` into chunks of at most `max_len` bytes, the way Telegram's
/// message-length limit requires. A naive byte-offset split can land inside
/// an open `<b>`/`<i>`/`<code>`/`<pre>`/`<a href="...">` span produced by
/// this file's HTML conversion, leaving unbalanced HTML in both chunks —
/// Telegram's `sendMessage` then rejects the message with a 400. This closes
/// any span still open at the end of a chunk and reopens it at the start of
/// the next one, so every chunk is independently balanced HTML.
fn chunk_message(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }

    let tags = scan_html_tags(text);
    let mut tag_idx = 0;
    // Tags open going into the chunk currently being built, in the order
    // they were opened, as (open_text, close_text) pairs.
    let mut open_stack: Vec<(String, String)> = Vec::new();

    let mut chunks = vec![];
    let mut start = 0;

    while start < text.len() {
        let prefix: String = open_stack.iter().map(|(open, _)| open.as_str()).collect();
        // Reserve room for closing whatever's already open (`open_stack`)
        // plus one more span that might open partway through this chunk and
        // not close before the boundary. The tags this file emits never
        // nest, so at most one span is ever open at once — this covers it.
        let reserve: usize = open_stack
            .iter()
            .map(|(_, close)| close.len())
            .sum::<usize>()
            + MAX_CLOSE_TAG_LEN;
        let budget = max_len.saturating_sub(prefix.len() + reserve).max(1);

        let mut end = (start + budget).min(text.len());
        // Don't split in the middle of a UTF-8 character.
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        // Try to split at a newline for cleaner output.
        if end < text.len()
            && let Some(newline_pos) = text[start..end].rfind('\n')
        {
            end = start + newline_pos + 1;
        }

        // Don't split in the middle of a tag's own markup; track which tags
        // remain open across the boundary.
        while tag_idx < tags.len() && tags[tag_idx].start < end {
            let tag = &tags[tag_idx];
            if tag.end > end {
                if tag.start <= start {
                    // Not even this tag's own markup fits in the remaining
                    // budget (e.g. an extremely long URL). Include it whole
                    // rather than truncate it and emit broken markup — this
                    // chunk may exceed `max_len`, but only in this
                    // pathological case.
                    end = tag.end;
                    if tag.open {
                        open_stack.push((tag.open_text.clone(), tag.close_text.clone()));
                    } else if !open_stack.is_empty() {
                        open_stack.pop();
                    }
                    tag_idx += 1;
                } else {
                    // This tag's markup straddles the boundary — exclude it
                    // from this chunk entirely rather than truncate it.
                    end = tag.start;
                }
                break;
            }
            if tag.open {
                open_stack.push((tag.open_text.clone(), tag.close_text.clone()));
            } else if !open_stack.is_empty() {
                open_stack.pop();
            }
            tag_idx += 1;
        }

        if end < start {
            // Not reachable given the above, but guard against any future
            // change introducing zero/negative progress and hanging.
            end = start;
        }

        let mut chunk = String::with_capacity(prefix.len() + (end - start) + reserve);
        chunk.push_str(&prefix);
        chunk.push_str(&text[start..end]);
        for (_, close) in open_stack.iter().rev() {
            chunk.push_str(close);
        }

        chunks.push(chunk);
        start = end;
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal balance checker for the small tag set this file's HTML
    /// conversion emits. Returns true if every opening tag has a matching
    /// closing tag, in order, with nothing left open at the end.
    fn is_balanced_html(s: &str) -> bool {
        let simple: &[(&str, &str)] = &[
            ("<b>", "</b>"),
            ("<i>", "</i>"),
            ("<code>", "</code>"),
            ("<pre>", "</pre>"),
        ];
        let mut stack: Vec<&str> = Vec::new();
        let mut i = 0;
        let bytes = s.as_bytes();
        while i < s.len() {
            if bytes[i] == b'<' {
                let rest = &s[i..];
                if let Some((open, close)) = simple.iter().find(|(open, _)| rest.starts_with(open))
                {
                    stack.push(close);
                    i += open.len();
                    continue;
                }
                if let Some((_, close)) = simple.iter().find(|(_, close)| rest.starts_with(close)) {
                    match stack.pop() {
                        Some(top) if top == *close => {}
                        _ => return false,
                    }
                    i += close.len();
                    continue;
                }
                if rest.starts_with("</a>") {
                    match stack.pop() {
                        Some("</a>") => {}
                        _ => return false,
                    }
                    i += 4;
                    continue;
                }
                if rest.starts_with("<a href=\"")
                    && let Some(rel) = rest.find("\">")
                {
                    stack.push("</a>");
                    i += rel + 2;
                    continue;
                }
            }
            i += 1;
            while i < s.len() && !s.is_char_boundary(i) {
                i += 1;
            }
        }
        stack.is_empty()
    }

    #[test]
    fn test_chunk_message_short_passthrough() {
        let short = "<b>hi</b>".to_string();
        assert_eq!(chunk_message(&short, 4096), vec![short]);
    }

    #[test]
    fn test_chunk_message_bold_span_straddles_boundary() {
        // A single <b>...</b> span longer than one chunk gets split across
        // chunks. Each resulting chunk must independently be balanced HTML
        // (the span closed at the end of the chunk it's cut in, and
        // reopened at the start of the next).
        let bold_text = "x".repeat(4090);
        let message = format!("start <b>{}</b> end", bold_text);
        let chunks = chunk_message(&message, 4096);

        assert!(
            chunks.len() > 1,
            "expected the message to be split into multiple chunks"
        );
        for chunk in &chunks {
            assert!(
                chunk.len() <= 4096,
                "chunk exceeds max_len: {}",
                chunk.len()
            );
            assert!(
                is_balanced_html(chunk),
                "chunk is not balanced HTML: {:?}",
                chunk
            );
        }
    }

    #[test]
    fn test_chunk_message_many_tags_stay_balanced() {
        let mut message = String::new();
        for i in 0..500 {
            message.push_str(&format!(
                "<b>bold{i}</b> and <a href=\"https://example.com/{i}\">link{i}</a> and <code>code{i}</code> filler text to pad things out. "
            ));
        }
        let chunks = chunk_message(&message, 300);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(
                is_balanced_html(chunk),
                "chunk is not balanced HTML: {:?}",
                chunk
            );
        }
    }
}
