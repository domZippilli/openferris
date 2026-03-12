use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::client;
use crate::config::GmailConfig;
use crate::email;
use crate::protocol::{DaemonRequest, RequestKind};
use crate::storage::Storage;

/// Maximum email body length sent to the daemon (chars).
const MAX_BODY_LEN: usize = 4000;

// --- Persistent state ---

#[derive(Debug, Default, Serialize, Deserialize)]
struct GmailState {
    history_id: Option<String>,
    our_email: Option<String>,
    /// thread_id -> unix timestamp of last reply
    thread_reply_timestamps: HashMap<String, i64>,
}

impl GmailState {
    fn path() -> PathBuf {
        crate::config::data_dir().join("gmail_state.json")
    }

    fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    fn save(&mut self) {
        // Prune old thread timestamps (>24h)
        let cutoff = chrono::Utc::now().timestamp() - 86400;
        self.thread_reply_timestamps.retain(|_, ts| *ts > cutoff);

        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("json.tmp");
        if let Ok(data) = serde_json::to_string_pretty(self) {
            if std::fs::write(&tmp, &data).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    fn is_rate_limited(&self, thread_id: &str, rate_limit_secs: u64) -> bool {
        if let Some(ts) = self.thread_reply_timestamps.get(thread_id) {
            let now = chrono::Utc::now().timestamp();
            (now - ts) < rate_limit_secs as i64
        } else {
            false
        }
    }

    fn record_reply(&mut self, thread_id: &str) {
        let now = chrono::Utc::now().timestamp();
        self.thread_reply_timestamps
            .insert(thread_id.to_string(), now);
        self.save();
    }
}

// --- Main entry point ---

pub async fn run(daemon_address: String, gmail_config: GmailConfig) -> Result<()> {
    let mut state = GmailState::load();

    let db_path = crate::config::data_dir().join("openferris.db");
    let storage = Storage::open(&db_path)?;

    tracing::info!("Gmail listener starting...");

    // Get our email address and seed history ID if needed
    let profile = run_gws(&["gmail", "users", "getProfile", "--params", &format!(r#"{{"userId":"me"}}"#)]).await?;
    let our_email = profile
        .get("emailAddress")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Could not determine our email address from getProfile"))?
        .to_lowercase();

    tracing::info!("Gmail account: {}", our_email);
    state.our_email = Some(our_email.clone());

    if state.history_id.is_none() {
        let hid = profile
            .get("historyId")
            .and_then(|v| v.as_str().or_else(|| v.as_u64().map(|_| "")))
            .map(|_| {
                // historyId can be a string or number in the response
                profile["historyId"].to_string().trim_matches('"').to_string()
            })
            .ok_or_else(|| anyhow::anyhow!("No historyId in getProfile response"))?;
        tracing::info!("Seeded initial historyId: {}", hid);
        state.history_id = Some(hid);
        state.save();
    }

    let poll_interval = std::time::Duration::from_secs(gmail_config.poll_interval_secs);
    let auth_backoff = std::time::Duration::from_secs(300); // 5 min backoff on auth errors
    let mut auth_failed = false;

    loop {
        if let Err(e) = poll_once(&daemon_address, &gmail_config, &storage, &mut state, &our_email).await {
            let err_str = format!("{:#}", e);
            if err_str.contains("401") || err_str.contains("authError") || err_str.contains("invalid_grant") {
                if !auth_failed {
                    tracing::error!("Gmail authentication expired. Run `gws auth login` to re-authenticate.");
                    auth_failed = true;
                }
                tokio::time::sleep(auth_backoff).await;
                continue;
            }
            auth_failed = false;
            tracing::error!("Poll error: {:#}", e);
        } else {
            auth_failed = false;
        }
        tokio::time::sleep(poll_interval).await;
    }
}

async fn poll_once(
    daemon_address: &str,
    config: &GmailConfig,
    storage: &Storage,
    state: &mut GmailState,
    our_email: &str,
) -> Result<()> {
    tracing::info!("Gmail: polling for new messages");

    let history_id = state
        .history_id
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No historyId"))?
        .clone();

    let params = format!(
        r#"{{"userId":"me","startHistoryId":"{}","historyTypes":"messageAdded","labelId":"INBOX"}}"#,
        history_id
    );

    let result = run_gws(&["gmail", "users", "history", "list", "--params", &params]).await;

    let response = match result {
        Ok(v) => v,
        Err(e) => {
            let err_str = format!("{:#}", e);
            if err_str.contains("404") {
                // Expired historyId — re-seed
                tracing::warn!("historyId expired, re-seeding from getProfile");
                let profile = run_gws(&["gmail", "users", "getProfile", "--params", r#"{"userId":"me"}"#]).await?;
                let hid = profile["historyId"]
                    .to_string()
                    .trim_matches('"')
                    .to_string();
                state.history_id = Some(hid);
                state.save();
                return Ok(());
            }
            return Err(e);
        }
    };

    // Update historyId
    if let Some(new_hid) = response.get("historyId") {
        let hid = new_hid.to_string().trim_matches('"').to_string();
        state.history_id = Some(hid);
        state.save();
    }

    // Collect new message IDs
    let mut message_ids: Vec<String> = vec![];
    if let Some(history) = response.get("history").and_then(|v| v.as_array()) {
        for entry in history {
            if let Some(added) = entry.get("messagesAdded").and_then(|v| v.as_array()) {
                for msg in added {
                    if let Some(id) = msg.get("message").and_then(|m| m.get("id")).and_then(|v| v.as_str()) {
                        if !message_ids.contains(&id.to_string()) {
                            message_ids.push(id.to_string());
                        }
                    }
                }
            }
        }
    }

    if message_ids.is_empty() {
        return Ok(());
    }

    tracing::info!("Gmail: {} new message(s)", message_ids.len());

    for msg_id in &message_ids {
        if let Err(e) = process_message(daemon_address, config, storage, state, our_email, msg_id).await {
            tracing::error!("Error processing message {}: {:#}", msg_id, e);
        }
    }

    Ok(())
}

async fn process_message(
    daemon_address: &str,
    config: &GmailConfig,
    storage: &Storage,
    state: &mut GmailState,
    our_email: &str,
    msg_id: &str,
) -> Result<()> {
    let params = format!(r#"{{"userId":"me","id":"{}","format":"full"}}"#, msg_id);
    let message = run_gws(&["gmail", "users", "messages", "get", "--params", &params]).await?;

    let headers = message
        .get("payload")
        .and_then(|p| p.get("headers"))
        .and_then(|h| h.as_array());

    let headers = match headers {
        Some(h) => h,
        None => {
            tracing::warn!("Message {} has no headers", msg_id);
            return Ok(());
        }
    };

    let from = extract_header(headers, "From").unwrap_or_default();
    let subject = extract_header(headers, "Subject").unwrap_or_default();
    let message_id_header = extract_header(headers, "Message-ID").unwrap_or_default();
    let references = extract_header(headers, "References").unwrap_or_default();
    let thread_id = message
        .get("threadId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let sender_email = email::parse_email_address(&from);

    // Skip our own messages
    if sender_email == our_email {
        return Ok(());
    }

    // Authorization check: config allowlist or known contact in SQLite
    let allowed = config.allowed_senders.iter().any(|s| s.to_lowercase() == sender_email)
        || storage.is_contact(&sender_email)?;

    if !allowed {
        tracing::info!("Gmail: skipping message from unauthorized sender");
        return Ok(());
    }

    // Rate limit check
    if state.is_rate_limited(&thread_id, config.rate_limit_secs) {
        tracing::info!("Gmail: rate limited on thread {}", thread_id);
        return Ok(());
    }

    // Extract body
    let body = extract_plain_text_body(&message)
        .or_else(|| {
            message
                .get("snippet")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();

    if body.is_empty() {
        tracing::info!("Gmail: skipping message with empty body");
        return Ok(());
    }

    // Truncate long emails
    let body = if body.len() > MAX_BODY_LEN {
        let mut end = MAX_BODY_LEN;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n\n[Email truncated — original was {} chars]", &body[..end], body.len())
    } else {
        body
    };

    let context = format!(
        "From: {}\nSubject: {}\n\n{}",
        from, subject, body
    );

    tracing::info!("Gmail: processing email from {} re: {}", sender_email, subject);

    // Send to daemon
    let request = DaemonRequest {
        id: uuid::Uuid::new_v4().to_string(),
        kind: RequestKind::RunSkill {
            skill_name: "email-reply".to_string(),
            context: Some(context),
        },
        source: Some("gmail".to_string()),
    };

    let response = match client::send_request(daemon_address, &request).await {
        Ok(text) => text,
        Err(e) => {
            tracing::error!("Daemon error for email reply: {:#}", e);
            return Ok(());
        }
    };

    // Compose and send the reply
    let reply_subject = if subject.starts_with("Re:") || subject.starts_with("RE:") {
        subject.clone()
    } else {
        format!("Re: {}", subject)
    };

    email::send_email(
        storage,
        &config.allowed_senders,
        Some(our_email),
        &from,
        &reply_subject,
        &response,
        Some(&message_id_header),
        Some(&references),
        Some(&thread_id),
    )
    .await?;

    tracing::info!("Gmail: sent reply in thread {}", thread_id);

    state.record_reply(&thread_id);

    Ok(())
}

// --- Helpers ---

async fn run_gws(args: &[&str]) -> Result<serde_json::Value> {
    let output = tokio::process::Command::new("gws")
        .args(args)
        .output()
        .await
        .context("Failed to run gws")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "gws {} exited with {}: {}{}",
            args.first().unwrap_or(&""),
            output.status,
            stdout,
            if stderr.is_empty() {
                String::new()
            } else {
                format!("\n{}", stderr)
            }
        );
    }

    serde_json::from_str(stdout.trim()).context("Failed to parse gws output as JSON")
}

fn extract_header(headers: &[serde_json::Value], name: &str) -> Option<String> {
    headers.iter().find_map(|h| {
        let hname = h.get("name")?.as_str()?;
        if hname.eq_ignore_ascii_case(name) {
            h.get("value")?.as_str().map(|s| s.to_string())
        } else {
            None
        }
    })
}

fn extract_plain_text_body(message: &serde_json::Value) -> Option<String> {
    let payload = message.get("payload")?;
    // Try top-level body first (simple messages)
    if let Some(mime) = payload.get("mimeType").and_then(|v| v.as_str()) {
        if mime == "text/plain" {
            if let Some(data) = payload.get("body").and_then(|b| b.get("data")).and_then(|d| d.as_str()) {
                return decode_base64url(data);
            }
        }
    }
    // Walk parts recursively
    extract_text_from_parts(payload)
}

fn extract_text_from_parts(part: &serde_json::Value) -> Option<String> {
    if let Some(parts) = part.get("parts").and_then(|p| p.as_array()) {
        for p in parts {
            let mime = p.get("mimeType").and_then(|v| v.as_str()).unwrap_or("");
            if mime == "text/plain" {
                if let Some(data) = p.get("body").and_then(|b| b.get("data")).and_then(|d| d.as_str()) {
                    return decode_base64url(data);
                }
            }
            // Recurse into nested parts (multipart/alternative, etc.)
            if mime.starts_with("multipart/") {
                if let Some(text) = extract_text_from_parts(p) {
                    return Some(text);
                }
            }
        }
    }
    None
}

fn decode_base64url(data: &str) -> Option<String> {
    URL_SAFE_NO_PAD
        .decode(data)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_header() {
        let headers: Vec<serde_json::Value> = serde_json::from_str(
            r#"[{"name":"From","value":"test@example.com"},{"name":"Subject","value":"Hello"}]"#,
        )
        .unwrap();

        assert_eq!(
            extract_header(&headers, "From"),
            Some("test@example.com".to_string())
        );
        assert_eq!(
            extract_header(&headers, "subject"),
            Some("Hello".to_string())
        );
        assert_eq!(extract_header(&headers, "Missing"), None);
    }

    #[test]
    fn test_rate_limit() {
        let mut state = GmailState::default();
        assert!(!state.is_rate_limited("thread1", 300));

        state.record_reply("thread1");
        assert!(state.is_rate_limited("thread1", 300));
        assert!(!state.is_rate_limited("thread2", 300));
    }

    #[test]
    fn test_decode_base64url() {
        // "Hello, World!" in base64url
        let encoded = URL_SAFE_NO_PAD.encode(b"Hello, World!");
        assert_eq!(decode_base64url(&encoded), Some("Hello, World!".to_string()));
    }
}
