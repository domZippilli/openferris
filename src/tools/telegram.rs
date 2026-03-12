use anyhow::Result;
use async_trait::async_trait;

use super::Tool;

pub struct SendTelegramTool {
    bot_token: String,
    default_chat_id: Option<i64>,
}

impl SendTelegramTool {
    pub fn new(bot_token: String, default_chat_id: Option<i64>) -> Self {
        Self {
            bot_token,
            default_chat_id,
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
         Write your message as plain text with simple markdown: *bold*, _italic_, `code`, ```code blocks```. \
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
    let text = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");

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

        // *bold*
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

        // _italic_
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

fn chunk_message(text: &str, max_len: usize) -> Vec<&str> {
    if text.len() <= max_len {
        return vec![text];
    }

    let mut chunks = vec![];
    let mut start = 0;

    while start < text.len() {
        let mut end = (start + max_len).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end < text.len() {
            if let Some(newline_pos) = text[start..end].rfind('\n') {
                end = start + newline_pos + 1;
            }
        }
        chunks.push(&text[start..end]);
        start = end;
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_text_passthrough() {
        assert_eq!(
            markdown_to_html("Hello, world!"),
            "Hello, world!"
        );
    }

    #[test]
    fn test_bold() {
        assert_eq!(
            markdown_to_html("This is *bold* text"),
            "This is <b>bold</b> text"
        );
    }

    #[test]
    fn test_italic() {
        assert_eq!(
            markdown_to_html("This is _italic_ text"),
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
        assert_eq!(
            markdown_to_html("5 * 3 = 15"),
            "5 * 3 = 15"
        );
    }

    #[test]
    fn test_complex_message() {
        let input = "*Summary*\n\n3 meetings (9am, 11am, 2pm).\nReview PR #42.\nScore: 8/10.";
        let expected = "<b>Summary</b>\n\n3 meetings (9am, 11am, 2pm).\nReview PR #42.\nScore: 8/10.";
        assert_eq!(markdown_to_html(input), expected);
    }
}
