use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::storage::Storage;

/// Send an email via gws, checking the `to` recipient against the allowed
/// senders config list and the known_contacts table in SQLite.
///
/// If `allowed_senders` is empty, all recipients are allowed.
/// After sending, the `to` recipient is recorded as a known contact.
///
/// `cc` is split into two trust tiers:
/// - `vetted_cc`: addresses the caller has already approved through its own
///   logic (e.g. config-sourced `always_cc`, or the Gmail reply-all path's
///   `approved_thread_recipients`, which only admits addresses introduced to
///   the thread by the owner or a whitelisted sender). These are sent
///   verbatim and are neither re-authorized against the allowlist/contacts
///   nor recorded as contacts, since they may legitimately include third
///   parties who are not themselves whitelisted.
/// - `unvetted_cc`: addresses supplied directly by a caller that has not
///   done its own vetting (e.g. a model/tool-call parameter). These are
///   authorized against the allowlist/known-contacts exactly like `to`.
pub async fn send_email(
    storage: &Storage,
    allowed_senders: &[String],
    owner_emails: &[String],
    from: Option<&str>,
    to: &str,
    vetted_cc: Option<&str>,
    unvetted_cc: Option<&str>,
    subject: &str,
    body: &str,
    in_reply_to: Option<&str>,
    references: Option<&str>,
    thread_id: Option<&str>,
    content_type: Option<&str>,
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

        authorize_cc(unvetted_cc, allowed_senders, storage)?;
    }

    let cc = merge_cc(vetted_cc, normalize_cc(unvetted_cc).as_deref());

    // Compose RFC 2822. The To header uses the parsed, authorized address —
    // never the raw `to` string, which for "Name <addr>, extra@evil" would
    // smuggle an unauthorized recipient past the single-address check above.
    let raw = compose_raw(
        from,
        &recipient,
        cc.as_deref(),
        subject,
        body,
        in_reply_to,
        references,
        content_type,
    );
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

    // Thread persistence: the recipient's counterparty thread gets this send
    // as an outbound chat turn (owner email -> the shared "owner" thread;
    // anyone else -> their own "email:<addr>" thread). Best-effort: a logging
    // failure shouldn't fail a send that already went out.
    let counterparty = crate::counterparty::email_counterparty(&recipient, owner_emails);
    if let Err(e) = storage.append_message(
        &counterparty,
        "email",
        crate::storage::DIRECTION_OUTBOUND,
        crate::storage::KIND_CHAT,
        &crate::storage::outbound_tag("email", body),
    ) {
        tracing::warn!(
            "Failed to append outbound email to thread {}: {}",
            counterparty,
            e
        );
    }

    tracing::info!("Email sent to {}", recipient);

    Ok(())
}

/// Non-async version for use in tool contexts where Storage isn't Send.
/// Opens its own DB connection, checks contacts, sends, records contact.
///
/// See [`send_email`] for the `vetted_cc`/`unvetted_cc` trust split.
pub async fn send_email_with_db(
    db_path: &std::path::Path,
    allowed_senders: &[String],
    owner_emails: &[String],
    to: &str,
    vetted_cc: Option<&str>,
    unvetted_cc: Option<&str>,
    subject: &str,
    body: &str,
    content_type: Option<&str>,
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

        authorize_cc(unvetted_cc, allowed_senders, &storage)?;
    }

    let cc = merge_cc(vetted_cc, normalize_cc(unvetted_cc).as_deref());

    // See send_email: the To header must use the parsed, authorized address.
    let raw = compose_raw(
        None,
        &recipient,
        cc.as_deref(),
        subject,
        body,
        None,
        None,
        content_type,
    );
    let encoded = URL_SAFE_NO_PAD.encode(raw.as_bytes());
    let send_body = serde_json::json!({ "raw": encoded });

    run_gws_send(&send_body.to_string()).await?;

    // Record contact + thread persistence with a fresh connection (after the await).
    let storage = Storage::open(db_path)?;
    storage.add_contact(&recipient)?;

    let counterparty = crate::counterparty::email_counterparty(&recipient, owner_emails);
    if let Err(e) = storage.append_message(
        &counterparty,
        "email",
        crate::storage::DIRECTION_OUTBOUND,
        crate::storage::KIND_CHAT,
        &crate::storage::outbound_tag("email", body),
    ) {
        tracing::warn!(
            "Failed to append outbound email to thread {}: {}",
            counterparty,
            e
        );
    }

    tracing::info!("Email sent to {}", recipient);

    Ok(())
}

/// Authorize every address in a comma-separated `cc` list (model/tool-call
/// supplied, i.e. not otherwise vetted by the caller) against the allowed
/// senders config list or the known_contacts table, mirroring the `to` check.
/// Assumes `allowed_senders` is non-empty (callers only invoke this inside
/// that guard, matching the `to` authorization).
fn authorize_cc(cc: Option<&str>, allowed_senders: &[String], storage: &Storage) -> Result<()> {
    let Some(cc) = cc else {
        return Ok(());
    };

    for addr in cc.split(',') {
        let addr = addr.trim();
        if addr.is_empty() {
            continue;
        }

        let recipient = parse_email_address(addr);
        let in_allowlist = allowed_senders
            .iter()
            .any(|s| s.to_lowercase() == recipient);
        let is_known = storage.is_contact(&recipient)?;

        if !in_allowlist && !is_known {
            bail!(
                "Cc recipient '{}' is not in the allowed senders list or known contacts",
                recipient
            );
        }
    }

    Ok(())
}

/// Reduce an unvetted, comma-separated cc list to the bare parsed addresses
/// (the same form `authorize_cc` checked), so display-name markup in the raw
/// input can't carry anything past authorization into the Cc header.
fn normalize_cc(cc: Option<&str>) -> Option<String> {
    let addrs: Vec<String> = cc?
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(parse_email_address)
        .collect();

    if addrs.is_empty() {
        None
    } else {
        Some(addrs.join(", "))
    }
}

/// Merge a caller-vetted cc list with an unvetted (but now authorized) cc
/// list into a single comma-separated string for `compose_raw`.
fn merge_cc(vetted_cc: Option<&str>, unvetted_cc: Option<&str>) -> Option<String> {
    let parts: Vec<&str> = [unvetted_cc, vetted_cc]
        .into_iter()
        .flatten()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// Sanitize a single header value against CRLF/header injection by
/// collapsing embedded `\r`, `\n`, and `\r\n` sequences to a single space.
/// Reply subjects and In-Reply-To/References values derive from inbound
/// (attacker-controlled) email headers, so this must run on every value
/// interpolated into a raw header line.
fn sanitize_header_value(value: &str) -> String {
    value.replace("\r\n", " ").replace(['\r', '\n'], " ")
}

fn compose_raw(
    from: Option<&str>,
    to: &str,
    cc: Option<&str>,
    subject: &str,
    body: &str,
    in_reply_to: Option<&str>,
    references: Option<&str>,
    content_type: Option<&str>,
) -> String {
    let mut msg = String::new();
    if let Some(from) = from {
        msg.push_str(&format!("From: {}\r\n", sanitize_header_value(from)));
    }
    msg.push_str(&format!("To: {}\r\n", sanitize_header_value(to)));
    if let Some(cc) = cc {
        if !cc.is_empty() {
            msg.push_str(&format!("Cc: {}\r\n", sanitize_header_value(cc)));
        }
    }
    msg.push_str(&format!("Subject: {}\r\n", sanitize_header_value(subject)));

    if let Some(irt) = in_reply_to {
        if !irt.is_empty() {
            let irt = sanitize_header_value(irt);
            msg.push_str(&format!("In-Reply-To: {}\r\n", irt));

            let refs = match references {
                Some(r) if !r.is_empty() => format!("{} {}", sanitize_header_value(r), irt),
                _ => irt.clone(),
            };
            msg.push_str(&format!("References: {}\r\n", refs));
        }
    }

    let ct = content_type.unwrap_or("text/plain");
    msg.push_str(&format!(
        "Content-Type: {}; charset=\"utf-8\"\r\n",
        sanitize_header_value(ct)
    ));
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
            None,
        );

        assert!(!raw.contains("From:"));
        assert!(raw.contains("To: them@example.com\r\n"));
        assert!(!raw.contains("Cc:"));
        assert!(raw.contains("Subject: Hello\r\n"));
        assert!(!raw.contains("In-Reply-To"));
        assert!(raw.contains("Content-Type: text/plain"));
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
            None,
        );

        assert!(raw.contains("In-Reply-To: <abc@mail>\r\n"));
        assert!(raw.contains("References: <prev@mail> <abc@mail>\r\n"));
    }

    #[test]
    fn test_compose_raw_html() {
        let raw = compose_raw(
            None,
            "them@example.com",
            None,
            "Briefing",
            "<h2>Hello</h2><p>World</p>",
            None,
            None,
            Some("text/html"),
        );

        assert!(raw.contains("Content-Type: text/html; charset=\"utf-8\""));
        assert!(raw.contains("<h2>Hello</h2><p>World</p>"));
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
        // Recipient smuggling: only the angle-bracket address survives
        // parsing, and compose gets this parsed form — the trailing extra
        // address never reaches the To header.
        assert_eq!(
            parse_email_address("Boss <boss@example.com>, evil@example.com"),
            "boss@example.com"
        );
    }

    #[test]
    fn test_normalize_cc_strips_display_name_markup() {
        assert_eq!(
            normalize_cc(Some("Pal <pal@example.com>, plain@example.com")).as_deref(),
            Some("pal@example.com, plain@example.com")
        );
        assert_eq!(normalize_cc(Some("  ,  ")), None);
        assert_eq!(normalize_cc(None), None);
    }

    #[test]
    fn test_authorize_cc_rejects_unallowed_allows_allowed() {
        let storage = Storage::open(&std::path::PathBuf::from(":memory:")).unwrap();
        let allowed_senders = vec!["boss@example.com".to_string()];

        // Not in the allowlist and not a known contact -> rejected.
        let err =
            authorize_cc(Some("attacker@example.com"), &allowed_senders, &storage).unwrap_err();
        assert!(err.to_string().contains("attacker@example.com"));

        // In the config allowlist -> passes.
        assert!(authorize_cc(Some("boss@example.com"), &allowed_senders, &storage).is_ok());

        // A known contact (not in the config allowlist) -> also passes.
        storage.add_contact("friend@example.com").unwrap();
        assert!(authorize_cc(Some("friend@example.com"), &allowed_senders, &storage).is_ok());

        // Mixed list: one allowed, one not -> rejected as a whole.
        let err = authorize_cc(
            Some("boss@example.com, attacker@example.com"),
            &allowed_senders,
            &storage,
        )
        .unwrap_err();
        assert!(err.to_string().contains("attacker@example.com"));
    }

    #[test]
    fn test_compose_raw_sanitizes_crlf_header_injection() {
        let raw = compose_raw(
            None,
            "them@example.com",
            None,
            "evil\r\nBcc: attacker@example.com",
            "Hi!",
            None,
            None,
            None,
        );

        // No new header line should have been injected into the header
        // section (everything before the blank line separating headers from
        // the body).
        let (headers, _) = raw.split_once("\r\n\r\n").expect("header/body split");
        for line in headers.split("\r\n") {
            assert!(
                !line.to_lowercase().starts_with("bcc:"),
                "injected header line found: {line:?}"
            );
        }

        // The CRLF was collapsed into the Subject value rather than starting
        // a new header.
        assert!(raw.contains("Subject: evil Bcc: attacker@example.com\r\n"));
        assert!(!raw.contains("\r\nBcc:"));
    }
}
