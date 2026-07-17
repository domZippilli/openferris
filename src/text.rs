//! Shared text-truncation primitives.
//!
//! Several call sites (storage and the Gmail listener) each
//! grew their own copy of a "truncate this string so it doesn't blow up
//! context/a message limit" loop, with byte- and char-based semantics mixed
//! together under similar-sounding names. This module centralizes the two
//! *distinct* semantics as explicitly named functions:
//!
//! - [`truncate_bytes`]: cap at N *bytes*, snapped back to the nearest UTF-8
//!   char boundary so multi-byte characters are never split.
//! - [`truncate_chars`]: cap at N Unicode scalar values (*chars*), regardless
//!   of byte length.
//!
//! Neither function appends a "truncated" marker itself — callers that want
//! one format and append their own (wording varies: "[truncated]", "[Email
//! truncated — original was N chars]", etc.), since that's the part of each
//! call site's behavior this refactor is explicitly preserving as-is.

/// Truncate `s` to at most `max_bytes` bytes, snapping back to the nearest
/// UTF-8 char boundary so multi-byte characters are never split. Returns `s`
/// unchanged (borrowed, no allocation) if it already fits.
pub fn truncate_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Truncate `s` to at most `max_chars` Unicode scalar values (not bytes).
/// Returns an owned `String` since char-counted truncation isn't generally
/// expressible as a borrowed subslice without re-walking the string anyway.
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_bytes_passthrough_when_within_budget() {
        assert_eq!(truncate_bytes("hello", 10), "hello");
        assert_eq!(truncate_bytes("hello", 5), "hello");
    }

    #[test]
    fn truncate_bytes_cuts_at_budget() {
        assert_eq!(truncate_bytes("hello world", 5), "hello");
    }

    #[test]
    fn truncate_bytes_snaps_back_to_char_boundary() {
        // 'é' is 2 bytes; a budget of 5 lands mid-char at byte 5 (chars: é é
        // é... each 2 bytes, so byte 5 is inside the 3rd char). Expect a
        // snap back to byte 4 (2 whole chars).
        let s = "ééééé"; // 10 bytes, 5 chars
        assert_eq!(truncate_bytes(s, 5), "éé");
        assert_eq!(truncate_bytes(s, 5).len(), 4);
    }

    #[test]
    fn truncate_chars_passthrough_when_within_budget() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello", 5), "hello");
    }

    #[test]
    fn truncate_chars_counts_scalars_not_bytes() {
        // 5 chars, 10 bytes; capping at 3 chars keeps 3 chars (6 bytes),
        // not 3 bytes (which would split a char).
        let s = "ééééé";
        assert_eq!(truncate_chars(s, 3), "ééé");
    }

    #[test]
    fn truncate_chars_ascii_cuts_at_char_count() {
        assert_eq!(truncate_chars("hello world", 5), "hello");
    }
}
