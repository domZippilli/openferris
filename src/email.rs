use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

use crate::storage::Storage;

/// Send an email via gws, checking the recipient against the allowed senders
/// config list and the known_contacts table in SQLite.
///
/// If `allowed_senders` is empty, all recipients are allowed.
/// After sending, the recipient is recorded as a known contact.
pub async fn send_email(
    storage: &Storage,
    allowed_senders: &[String],
    from: Option<&str>,
    to: &str,
    subject: &str,
    body: &str,
    in_reply_to: Option<&str>,
    references: Option<&str>,
    thread_id: Option<&str>,
) -> Result<()> {
    let recipient = parse_email_address(to);

    // Check authorization: config allowlist OR known contact
    if !allowed_senders.is_empty() {
        let in_allowlist = allowed_senders
            .iter()
            .any(|s| s.to_lowercase() == recipient);
        let is_known = storage.is_contact(&recipient)?;

        if !in_allowlist && !is_known {
            bail!(
                "Recipient '{}' is not in the allowed senders list or known contacts",
                recipient
            );
        }
    }

    // Compose RFC 2822
    let raw = compose_raw(from, to, None, subject, body, in_reply_to, references);
    let encoded = URL_SAFE_NO_PAD.encode(raw.as_bytes());

    let mut send_body = serde_json::json!({ "raw": encoded });
    if let Some(tid) = thread_id {
        send_body["threadId"] = serde_json::json!(tid);
    }

    // Send the email (async — do all Storage work before/after this)
    let json_str = send_body.to_string();
    run_gws_send(&json_str).await?;

    // Record the recipient as a known contact
    storage.add_contact(&recipient)?;

    tracing::info!("Email sent to {}", recipient);

    Ok(())
}

/// Non-async version for use in tool contexts where Storage isn't Send.
/// Opens its own DB connection, checks contacts, sends, records contact.
pub async fn send_email_with_db(
    db_path: &std::path::Path,
    allowed_senders: &[String],
    to: &str,
    cc: Option<&str>,
    subject: &str,
    body: &str,
) -> Result<()> {
    let recipient = parse_email_address(to);

    // Check authorization with a short-lived Storage connection
    if !allowed_senders.is_empty() {
        let storage = Storage::open(db_path)?;
        let in_allowlist = allowed_senders
            .iter()
            .any(|s| s.to_lowercase() == recipient);
        let is_known = storage.is_contact(&recipient)?;

        if !in_allowlist && !is_known {
            bail!(
                "Recipient '{}' is not in the allowed senders list or known contacts",
                recipient
            );
        }
    }

    let raw = compose_raw(None, to, cc, subject, body, None, None);
    let encoded = URL_SAFE_NO_PAD.encode(raw.as_bytes());
    let send_body = serde_json::json!({ "raw": encoded });

    run_gws_send(&send_body.to_string()).await?;

    // Record contact with a fresh connection (after the await)
    let storage = Storage::open(db_path)?;
    storage.add_contact(&recipient)?;

    tracing::info!("Email sent to {}", recipient);

    Ok(())
}

fn compose_raw(
    from: Option<&str>,
    to: &str,
    cc: Option<&str>,
    subject: &str,
    body: &str,
    in_reply_to: Option<&str>,
    references: Option<&str>,
) -> String {
    let mut msg = String::new();
    if let Some(from) = from {
        msg.push_str(&format!("From: {}\r\n", from));
    }
    msg.push_str(&format!("To: {}\r\n", to));
    if let Some(cc) = cc {
        if !cc.is_empty() {
            msg.push_str(&format!("Cc: {}\r\n", cc));
        }
    }
    msg.push_str(&format!("Subject: {}\r\n", subject));

    if let Some(irt) = in_reply_to {
        if !irt.is_empty() {
            msg.push_str(&format!("In-Reply-To: {}\r\n", irt));

            let refs = match references {
                Some(r) if !r.is_empty() => format!("{} {}", r, irt),
                _ => irt.to_string(),
            };
            msg.push_str(&format!("References: {}\r\n", refs));
        }
    }

    msg.push_str("Content-Type: text/plain; charset=\"utf-8\"\r\n");
    msg.push_str("MIME-Version: 1.0\r\n");
    msg.push_str("\r\n");
    msg.push_str(body);

    msg
}

async fn run_gws_send(json_body: &str) -> Result<()> {
    let output = tokio::process::Command::new("gws")
        .args([
            "gmail",
            "users",
            "messages",
            "send",
            "--params",
            r#"{"userId":"me"}"#,
            "--json",
            json_body,
        ])
        .output()
        .await
        .context("Failed to run gws")?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gws send failed: {}{}", stdout, stderr);
    }

    Ok(())
}

pub fn parse_email_address(from: &str) -> String {
    if let Some(start) = from.rfind('<') {
        if let Some(end) = from[start..].find('>') {
            return from[start + 1..start + end].to_lowercase();
        }
    }
    from.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compose_raw_simple() {
        let raw = compose_raw(
            None,
            "them@example.com",
            None,
            "Hello",
            "Hi there!",
            None,
            None,
        );

        assert!(!raw.contains("From:"));
        assert!(raw.contains("To: them@example.com\r\n"));
        assert!(!raw.contains("Cc:"));
        assert!(raw.contains("Subject: Hello\r\n"));
        assert!(!raw.contains("In-Reply-To"));
        assert!(raw.contains("Hi there!"));
    }

    #[test]
    fn test_compose_raw_with_from() {
        let raw = compose_raw(
            Some("me@example.com"),
            "them@example.com",
            None,
            "Hello",
            "Hi!",
            None,
            None,
        );

        assert!(raw.contains("From: me@example.com\r\n"));
    }

    #[test]
    fn test_compose_raw_with_cc() {
        let raw = compose_raw(
            None,
            "them@example.com",
            Some("boss@example.com"),
            "Hello",
            "Hi!",
            None,
            None,
        );

        assert!(raw.contains("To: them@example.com\r\n"));
        assert!(raw.contains("Cc: boss@example.com\r\n"));
        assert!(raw.contains("Subject: Hello\r\n"));
    }

    #[test]
    fn test_compose_raw_reply() {
        let raw = compose_raw(
            Some("me@example.com"),
            "them@example.com",
            None,
            "Re: Hello",
            "Thanks!",
            Some("<abc@mail>"),
            Some("<prev@mail>"),
        );

        assert!(raw.contains("In-Reply-To: <abc@mail>\r\n"));
        assert!(raw.contains("References: <prev@mail> <abc@mail>\r\n"));
    }

    #[test]
    fn test_parse_email_address() {
        assert_eq!(
            parse_email_address("John <john@example.com>"),
            "john@example.com"
        );
        assert_eq!(
            parse_email_address("plain@example.com"),
            "plain@example.com"
        );
    }
}
