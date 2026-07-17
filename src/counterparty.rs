//! Resolve an email identity
//! to a stable counterparty key for the per-counterparty message threads in
//! to a stable counterparty key for the per-counterparty message threads in
//! `storage.rs`.

/// The owner's thread key.
pub const OWNER: &str = "owner";

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
