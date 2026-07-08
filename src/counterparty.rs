//! Resolve a transport-specific identity (a Telegram chat, an email address)
//! to a stable counterparty key for the per-counterparty message threads in
//! `storage.rs`. Both the daemon (inbound routing) and tools (outbound sends)
//! need this, so it lives on the lib side rather than in a binary-only module.
//!
//! Two counterparty shapes:
//!   * `"owner"` — the configured owner, identified via Telegram
//!     `allowed_users`/`default_chat_id` or `[user] emails`.
//!   * `"email:<lowercased addr>"` — any other email address.
//!
//! There's no non-owner Telegram bucket in the design (the bot's transport
//! layer already rejects messages from chat/user ids outside
//! `allowed_users` before they ever reach the daemon), but `telegram_counterparty`
//! still returns a `"telegram:<chat_id>"` fallback for the defensive case of a
//! send targeting a chat_id that isn't recognized as the owner (e.g. an
//! explicit `chat_id` param on `send_telegram` that isn't the default chat).

/// The owner's thread key.
pub const OWNER: &str = "owner";

/// Resolve a Telegram chat to a counterparty key.
///
/// Telegram private chats have `chat_id == user_id`, so `allowed_users`
/// (documented as "chat/user ids") doubles as a chat-id check here — useful
/// for outbound sends where only a `chat_id` is known, not a `user_id`.
pub fn telegram_counterparty(
    chat_id: i64,
    default_chat_id: Option<i64>,
    allowed_users: &[u64],
) -> String {
    let is_owner =
        default_chat_id == Some(chat_id) || allowed_users.iter().any(|&u| u as i64 == chat_id);
    if is_owner {
        OWNER.to_string()
    } else {
        format!("telegram:{}", chat_id)
    }
}

/// Resolve an email address to a counterparty key: the owner's configured
/// email(s) map to `"owner"`; anyone else gets `"email:<lowercased addr>"`.
/// `addr` may be a bare address or a `"Display Name <addr>"` string — it's
/// parsed the same way inbound/outbound mail addresses are elsewhere.
pub fn email_counterparty(addr: &str, owner_emails: &[String]) -> String {
    let parsed = crate::email::parse_email_address(addr);
    if owner_emails.iter().any(|e| e.to_lowercase() == parsed) {
        OWNER.to_string()
    } else {
        format!("email:{}", parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_telegram_owner_via_default_chat_id() {
        assert_eq!(telegram_counterparty(555, Some(555), &[]), OWNER);
    }

    #[test]
    fn test_telegram_owner_via_allowed_users() {
        assert_eq!(telegram_counterparty(42, None, &[42, 99]), OWNER);
    }

    #[test]
    fn test_telegram_non_owner_falls_back_to_bucket() {
        assert_eq!(
            telegram_counterparty(7, Some(555), &[42]),
            "telegram:7".to_string()
        );
    }

    #[test]
    fn test_telegram_no_owner_config_at_all() {
        assert_eq!(
            telegram_counterparty(7, None, &[]),
            "telegram:7".to_string()
        );
    }

    #[test]
    fn test_email_owner_case_insensitive() {
        let owners = vec!["Me@Example.com".to_string()];
        assert_eq!(email_counterparty("me@example.com", &owners), OWNER);
        assert_eq!(
            email_counterparty("Display Name <ME@EXAMPLE.COM>", &owners),
            OWNER
        );
    }

    #[test]
    fn test_email_non_owner_bucketed_by_address() {
        let owners = vec!["me@example.com".to_string()];
        assert_eq!(
            email_counterparty("Stranger <Stranger@Other.com>", &owners),
            "email:stranger@other.com".to_string()
        );
    }

    #[test]
    fn test_email_no_owners_configured() {
        assert_eq!(
            email_counterparty("anyone@example.com", &[]),
            "email:anyone@example.com".to_string()
        );
    }
}
