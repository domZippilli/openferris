use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;

use super::Tool;
use crate::storage::Storage;

pub struct SendTelegramTool {
    bot_token: String,
    default_chat_id: Option<i64>,
    /// Telegram user/chat ids configured as the owner (see
    /// `[telegram].allowed_users`). Used to resolve which message thread
    /// (`crate::counterparty`) an outbound send belongs to.
    allowed_users: Vec<u64>,
    db_path: Option<PathBuf>,
}

impl SendTelegramTool {
    pub fn new(bot_token: String, default_chat_id: Option<i64>, allowed_users: Vec<u64>) -> Self {
        Self {
            bot_token,
            default_chat_id,
            allowed_users,
            db_path: None,
        }
    }

    pub fn new_with_storage(
        bot_token: String,
        default_chat_id: Option<i64>,
        allowed_users: Vec<u64>,
        db_path: PathBuf,
    ) -> Self {
        Self {
            bot_token,
            default_chat_id,
            allowed_users,
            db_path: Some(db_path),
        }
    }
}

#[async_trait]
impl Tool for SendTelegramTool {
    fn name(&self) -> &str {
        "send_telegram"
    }

    fn description_for_llm(&self) -> &str {
        "Send a message via Telegram. \
         Parameters: {\"message\": \"<text>\", \"chat_id\": <optional number>}. \
         If chat_id is omitted, the message is sent to the default configured chat. \
         Write your message as plain text with simple markdown: *bold*, _italic_, `code`, ```code blocks```, [link text](url). \
         Formatting is handled automatically — just write naturally. \
         Use this to deliver results, notifications, or replies to the user via Telegram."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let message = params
            .get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: message"))?;

        let chat_id = params
            .get("chat_id")
            .and_then(|v| v.as_i64())
            .or(self.default_chat_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No chat_id provided and no default_chat_id configured in [telegram]"
                )
            })?;

        let html = markdown_to_html(message);

        let base_url = format!("https://api.telegram.org/bot{}", self.bot_token);
        let client = reqwest::Client::new();

        // Telegram has a 4096 char limit per message
        let chunks = chunk_message(&html, 4096);

        for chunk in &chunks {
            let body = serde_json::json!({
                "chat_id": chat_id,
                "text": chunk,
                "parse_mode": "HTML",
            });

            let resp = client
                .post(format!("{}/sendMessage", base_url))
                .json(&body)
                .send()
                .await?;

            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Telegram API error: {}", body);
            }
        }

        if let Some(db_path) = &self.db_path {
            match Storage::open(db_path) {
                Ok(store) => {
                    if let Err(e) = store.log_interaction(
                        "telegram",
                        Some("send_telegram"),
                        &format!("Outbound Telegram message sent to chat {}", chat_id),
                        message,
                    ) {
                        tracing::warn!("Failed to log outbound Telegram message: {}", e);
                    }

                    // Thread persistence: this send is an outbound chat turn
                    // in the recipient's message thread (owner if chat_id
                    // resolves to the owner; otherwise a per-chat bucket).
                    let counterparty = crate::counterparty::telegram_counterparty(
                        chat_id,
                        self.default_chat_id,
                        &self.allowed_users,
                    );
                    if let Err(e) = store.append_message(
                        &counterparty,
                        "telegram",
                        crate::storage::DIRECTION_OUTBOUND,
                        crate::storage::KIND_CHAT,
                        &crate::storage::outbound_tag("telegram", message),
                    ) {
                        tracing::warn!(
                            "Failed to append outbound Telegram message to thread {}: {}",
                            counterparty,
                            e
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to open storage for Telegram delivery log: {}", e);
                }
            }
        }

        if chunks.len() == 1 {
            Ok("Message sent.".to_string())
        } else {
            Ok(format!("Message sent ({} parts).", chunks.len()))
        }
    }
}

/// Convert simple markdown to Telegram-compatible HTML.
///
/// Handles: *bold* → <b>, _italic_ → <i>, `code` → <code>, ```blocks``` → <pre>.
/// HTML entities (<, >, &) are escaped first so arbitrary text is safe.
fn markdown_to_html(text: &str) -> String {
    // Step 1: escape HTML entities
    let text = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");

    let mut result = String::with_capacity(text.len() * 2);
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        let c = chars[i];

        // ``` code blocks ```
        if c == '`' && i + 2 < len && chars[i + 1] == '`' && chars[i + 2] == '`' {
            i += 3;
            // Skip optional language tag and newline after opening ```
            while i < len && chars[i] != '\n' && chars[i] != '`' {
                i += 1;
            }
            if i < len && chars[i] == '\n' {
                i += 1;
            }
            result.push_str("<pre>");
            while i < len {
                if i + 2 < len && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
                    i += 3;
                    break;
                }
                result.push(chars[i]);
                i += 1;
            }
            // Trim trailing newline inside <pre>
            if result.ends_with('\n') {
                result.pop();
            }
            result.push_str("</pre>");
            continue;
        }

        // `inline code`
        if c == '`' {
            if let Some(end) = find_closing(&chars, i + 1, '`') {
                result.push_str("<code>");
                for j in (i + 1)..end {
                    result.push(chars[j]);
                }
                result.push_str("</code>");
                i = end + 1;
                continue;
            }
        }

        // [text](url) links
        if c == '[' {
            if let Some((text_end, url_start, url_end)) = find_markdown_link(&chars, i) {
                result.push_str("<a href=\"");
                for j in url_start..url_end {
                    result.push(chars[j]);
                }
                result.push_str("\">");
                for j in (i + 1)..text_end {
                    result.push(chars[j]);
                }
                result.push_str("</a>");
                i = url_end + 1; // skip past closing ')'
                continue;
            }
        }

        // **bold** (standard markdown — must check before single *)
        if c == '*' && i + 1 < len && chars[i + 1] == '*' {
            if let Some(end) = find_double_closing(&chars, i + 2, '*') {
                result.push_str("<b>");
                for j in (i + 2)..end {
                    result.push(chars[j]);
                }
                result.push_str("</b>");
                i = end + 2;
                continue;
            }
        }

        // *bold* (single asterisk — Telegram convention)
        if c == '*' {
            if let Some(end) = find_closing(&chars, i + 1, '*') {
                result.push_str("<b>");
                for j in (i + 1)..end {
                    result.push(chars[j]);
                }
                result.push_str("</b>");
                i = end + 1;
                continue;
            }
        }

        // __italic__ (standard markdown — must check before single _)
        if c == '_' && i + 1 < len && chars[i + 1] == '_' {
            if let Some(end) = find_double_closing(&chars, i + 2, '_') {
                result.push_str("<i>");
                for j in (i + 2)..end {
                    result.push(chars[j]);
                }
                result.push_str("</i>");
                i = end + 2;
                continue;
            }
        }

        // _italic_ (single underscore)
        if c == '_' {
            if let Some(end) = find_closing(&chars, i + 1, '_') {
                result.push_str("<i>");
                for j in (i + 1)..end {
                    result.push(chars[j]);
                }
                result.push_str("</i>");
                i = end + 1;
                continue;
            }
        }

        result.push(c);
        i += 1;
    }

    result
}

/// Parse a markdown link starting at `[` at position `start`.
/// Returns (text_end, url_start, url_end) where:
///   - text is chars[start+1..text_end]
///   - url is chars[url_start..url_end]
///   - the closing `)` is at url_end
fn find_markdown_link(chars: &[char], start: usize) -> Option<(usize, usize, usize)> {
    // Find closing ]
    let mut j = start + 1;
    while j < chars.len() && chars[j] != ']' && chars[j] != '\n' {
        j += 1;
    }
    if j >= chars.len() || chars[j] != ']' {
        return None;
    }
    let text_end = j;

    // Must be immediately followed by (
    if text_end + 1 >= chars.len() || chars[text_end + 1] != '(' {
        return None;
    }
    let url_start = text_end + 2;

    // Find closing )
    let mut k = url_start;
    while k < chars.len() && chars[k] != ')' && chars[k] != '\n' {
        k += 1;
    }
    if k >= chars.len() || chars[k] != ')' {
        return None;
    }

    // Don't match empty text or empty url
    if text_end == start + 1 || k == url_start {
        return None;
    }

    Some((text_end, url_start, k))
}

/// Find the position of a closing double delimiter (e.g. ** or __).
/// Returns the index of the first char of the closing pair.
fn find_double_closing(chars: &[char], start: usize, delim: char) -> Option<usize> {
    if start >= chars.len() {
        return None;
    }
    for j in start..chars.len() - 1 {
        if chars[j] == '\n' {
            return None;
        }
        if chars[j] == delim && chars[j + 1] == delim {
            if j == start {
                return None; // empty span
            }
            return Some(j);
        }
    }
    None
}

/// Find the position of a closing delimiter, ensuring it's not immediately after the opener
/// (to avoid matching empty spans like ** or __) and doesn't span across newlines
/// (to avoid matching unrelated markers).
fn find_closing(chars: &[char], start: usize, delim: char) -> Option<usize> {
    if start >= chars.len() || chars[start] == delim {
        return None; // empty span
    }
    for j in start..chars.len() {
        if chars[j] == '\n' {
            return None; // don't span lines
        }
        if chars[j] == delim {
            return Some(j);
        }
    }
    None
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

    #[test]
    fn test_plain_text_passthrough() {
        assert_eq!(markdown_to_html("Hello, world!"), "Hello, world!");
    }

    #[test]
    fn test_bold_single() {
        assert_eq!(
            markdown_to_html("This is *bold* text"),
            "This is <b>bold</b> text"
        );
    }

    #[test]
    fn test_bold_double() {
        assert_eq!(
            markdown_to_html("This is **bold** text"),
            "This is <b>bold</b> text"
        );
    }

    #[test]
    fn test_italic_single() {
        assert_eq!(
            markdown_to_html("This is _italic_ text"),
            "This is <i>italic</i> text"
        );
    }

    #[test]
    fn test_italic_double() {
        assert_eq!(
            markdown_to_html("This is __italic__ text"),
            "This is <i>italic</i> text"
        );
    }

    #[test]
    fn test_inline_code() {
        assert_eq!(
            markdown_to_html("Run `ls -la` now"),
            "Run <code>ls -la</code> now"
        );
    }

    #[test]
    fn test_code_block() {
        assert_eq!(
            markdown_to_html("```\ncode.here()\n```"),
            "<pre>code.here()</pre>"
        );
    }

    #[test]
    fn test_code_block_with_language() {
        assert_eq!(
            markdown_to_html("```rust\nfn main() {}\n```"),
            "<pre>fn main() {}</pre>"
        );
    }

    #[test]
    fn test_html_entities_escaped() {
        assert_eq!(
            markdown_to_html("if x < 10 && y > 5"),
            "if x &lt; 10 &amp;&amp; y &gt; 5"
        );
    }

    #[test]
    fn test_special_chars_passthrough() {
        // Characters that were problematic with MarkdownV2 should just work
        assert_eq!(
            markdown_to_html("Hello! Score: 5-3 (win)"),
            "Hello! Score: 5-3 (win)"
        );
    }

    #[test]
    fn test_mixed_formatting() {
        assert_eq!(
            markdown_to_html("*Bold* and _italic_ and `code`"),
            "<b>Bold</b> and <i>italic</i> and <code>code</code>"
        );
    }

    #[test]
    fn test_unpaired_markers_passthrough() {
        // A lone * or _ without a closing pair should pass through as-is
        assert_eq!(markdown_to_html("5 * 3 = 15"), "5 * 3 = 15");
    }

    #[test]
    fn test_complex_message() {
        let input = "*Summary*\n\n3 meetings (9am, 11am, 2pm).\nReview PR #42.\nScore: 8/10.";
        let expected =
            "<b>Summary</b>\n\n3 meetings (9am, 11am, 2pm).\nReview PR #42.\nScore: 8/10.";
        assert_eq!(markdown_to_html(input), expected);
    }

    #[test]
    fn test_link() {
        assert_eq!(
            markdown_to_html("[Click here](https://example.com)"),
            r#"<a href="https://example.com">Click here</a>"#
        );
    }

    #[test]
    fn test_link_with_special_url() {
        assert_eq!(
            markdown_to_html("[News](https://news.google.com/rss/articles/ABC_def-ghi)"),
            r#"<a href="https://news.google.com/rss/articles/ABC_def-ghi">News</a>"#
        );
    }

    #[test]
    fn test_link_with_bold_text() {
        // Bold inside link text isn't supported; use link + bold separately
        assert_eq!(
            markdown_to_html("[Headline](https://example.com) — *Source*"),
            r#"<a href="https://example.com">Headline</a> — <b>Source</b>"#
        );
    }

    #[test]
    fn test_bare_brackets_passthrough() {
        // Brackets without (url) should pass through
        assert_eq!(
            markdown_to_html("array[0] is the first element"),
            "array[0] is the first element"
        );
    }

    #[test]
    fn test_headline_format() {
        let input = "**Top Stories**\n\n- [Headline one](https://example.com/1) — AP News\n- [Headline two](https://example.com/2) — BBC";
        let expected = "<b>Top Stories</b>\n\n- <a href=\"https://example.com/1\">Headline one</a> — AP News\n- <a href=\"https://example.com/2\">Headline two</a> — BBC";
        assert_eq!(markdown_to_html(input), expected);
    }

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
