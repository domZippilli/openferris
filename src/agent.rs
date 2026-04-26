use anyhow::Result;
use tokio::sync::mpsc;

use crate::llm::{ChatMessage, LlmBackend, Role};
use crate::protocol::tool_progress_label;
use crate::skills::Skill;
use crate::tools::ToolRegistry;

const MAX_ITERATIONS: usize = 50;
/// Compact when estimated token count exceeds this fraction of the slot's
/// context window. The summarization call is an independent request (its own
/// 100% budget) so this threshold is really about headroom for the *next*
/// agent turn after compaction kicks the middle out.
const COMPACTION_THRESHOLD: f32 = 0.90;
/// Number of trailing (assistant + tool_result) pairs to keep verbatim. The
/// summary replaces everything older than this.
const KEEP_RECENT_PAIRS: usize = 3;
/// A run can compact at most this many times. Past that, something is wrong
/// (runaway tool output, infinite loop) and bailing is safer than masking it.
const MAX_COMPACTIONS: usize = 3;

#[derive(Clone, Debug)]
pub struct AgentResult {
    /// The response text to show the user (memory tags stripped).
    pub response: String,
    /// Facts the agent decided to remember.
    pub memories: Vec<String>,
}

pub struct Agent {
    llm: Box<dyn LlmBackend>,
    tools: ToolRegistry,
    soul: String,
}

impl Agent {
    pub fn new(llm: Box<dyn LlmBackend>, tools: ToolRegistry, soul: String) -> Self {
        Self { llm, tools, soul }
    }

    /// Run the agent loop for a skill with a user message.
    /// `history` contains prior conversation messages (for TUI sessions).
    /// `persistent_context` is loaded from storage (memories + recent interactions).
    pub async fn run(
        &self,
        skill: &Skill,
        user_message: &str,
        history: &[ChatMessage],
        identity: &str,
        user_profile: &str,
        persistent_context: &str,
        progress_tx: Option<mpsc::UnboundedSender<String>>,
    ) -> Result<AgentResult> {
        // Reset any per-run state held by tools (e.g. ask_claude session id).
        self.tools.notify_run_start();

        let system_prompt = self.build_system_prompt(skill, identity, user_profile, persistent_context);

        let mut messages = vec![ChatMessage {
            role: Role::System,
            content: system_prompt,
        }];

        // Append prior conversation history (TUI sessions)
        messages.extend_from_slice(history);

        // Append the current user message
        messages.push(ChatMessage {
            role: Role::User,
            content: user_message.to_string(),
        });

        // Discover the slot context window once per run (cached in the backend).
        let n_ctx = match self.llm.context_window_tokens().await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("Could not query context window: {}; falling back to 100_000", e);
                100_000
            }
        };
        let budget = ((n_ctx as f32) * COMPACTION_THRESHOLD) as usize;
        let mut compactions_done = 0usize;

        for iteration in 0..MAX_ITERATIONS {
            tracing::debug!("Agent iteration {}", iteration + 1);

            // Compact before each LLM call if we've exceeded budget.
            if compactions_done < MAX_COMPACTIONS && estimate_tokens(&messages) > budget {
                let before = estimate_tokens(&messages);
                match self.compact(&messages).await {
                    Ok(new_messages) => {
                        let after = estimate_tokens(&new_messages);
                        tracing::info!(
                            "Compacted context: {} -> {} tokens (compaction {}/{})",
                            before, after, compactions_done + 1, MAX_COMPACTIONS
                        );
                        messages = new_messages;
                        compactions_done += 1;
                    }
                    Err(e) => {
                        tracing::error!("Compaction failed: {}", e);
                        return Err(e);
                    }
                }
            }

            let response = self.llm.chat_completion(&messages).await?;
            tracing::debug!("LLM response: {}", response);
            let outcome = parse_tool_calls(&response);

            if outcome.calls.is_empty() && outcome.errors.is_empty() {
                // Final answer — extract memories and strip tags
                let memories = parse_memories(&response);
                let clean_response = strip_tags(&response);
                return Ok(AgentResult {
                    response: clean_response,
                    memories,
                });
            }

            // Add the assistant's response (with tool call markers) to history
            messages.push(ChatMessage {
                role: Role::Assistant,
                content: response,
            });

            // Execute each tool call and feed results back
            for call in &outcome.calls {
                tracing::info!("Tool call: {}", call.name);
                tracing::debug!("Tool params: {} {}", call.name, call.params);
                if let Some(ref tx) = progress_tx {
                    let _ = tx.send(tool_progress_label(&call.name).to_string());
                }
                let result = match self
                    .tools
                    .execute(&call.name, call.params.clone(), &skill.tools)
                    .await
                {
                    Ok(output) => {
                        tracing::info!("Tool result: {} ok ({} bytes)", call.name, output.len());
                        output
                    }
                    Err(e) => {
                        tracing::warn!("Tool error: {} — {}", call.name, e);
                        format!("Error: {}", e)
                    }
                };

                let body = match &call.repair_note {
                    Some(note) => format!("{}\n\n{}", note, result),
                    None => result,
                };
                messages.push(ChatMessage {
                    role: Role::User,
                    content: format!(
                        "<tool_result tool=\"{}\">\n{}\n</tool_result>",
                        call.name, body
                    ),
                });
            }

            // Feed parse failures back to the model so it can self-correct
            // instead of silently re-emitting the same malformed call.
            for err in &outcome.errors {
                tracing::warn!("Reporting tool_call parse error to model: {}", err.detail);
                let snippet = error_snippet(&err.raw, &err.detail);
                messages.push(ChatMessage {
                    role: Role::User,
                    content: format!(
                        "<tool_result tool=\"parse_error\">\nOne of your <tool_call> blocks could not be parsed and was NOT executed.\n\nParser error: {}\n\nNear the error: {}\n\nFix the JSON and re-emit the tool_call. Remember: `<` and `>` are plain characters in JSON strings — never prefix with `\\`.\n</tool_result>",
                        err.detail, snippet
                    ),
                });
            }
        }

        anyhow::bail!("Agent exceeded maximum iterations ({})", MAX_ITERATIONS)
    }

    /// Summarize the middle of `messages`, preserving the system prompt, the
    /// original user message (index 1, after history is folded in), and the
    /// last KEEP_RECENT_PAIRS (assistant + tool_result) pairs verbatim. The
    /// summary is produced by the same LLM backend.
    async fn compact(&self, messages: &[ChatMessage]) -> Result<Vec<ChatMessage>> {
        if messages.len() < 4 {
            // Nothing meaningful to compact.
            return Ok(messages.to_vec());
        }

        // Walk backward from the end. Each Assistant message marks the start
        // of a (assistant + its tool_results) pair. Stop once we've identified
        // the index of the KEEP_RECENT_PAIRS-th assistant from the end —
        // that's the boundary we keep verbatim.
        let mut keep_start = messages.len();
        let mut pairs_seen = 0usize;
        for (i, m) in messages.iter().enumerate().rev() {
            if matches!(m.role, Role::Assistant) {
                pairs_seen += 1;
                if pairs_seen == KEEP_RECENT_PAIRS {
                    keep_start = i;
                    break;
                }
            }
        }

        // Index 0 = system, index 1 = first user message; we always preserve
        // them so the model retains the task framing.
        let preserved_head_end = 2usize;
        if keep_start <= preserved_head_end {
            // Either fewer than KEEP_RECENT_PAIRS pairs exist, or all of them
            // are in the head we're already preserving. Nothing to summarize.
            return Ok(messages.to_vec());
        }
        let to_summarize = &messages[preserved_head_end..keep_start];

        let mut transcript = String::new();
        for m in to_summarize {
            transcript.push_str(&format!("[{}]\n{}\n\n", m.role.as_str(), m.content));
        }

        let summarization_messages = vec![
            ChatMessage {
                role: Role::System,
                content: "You are summarizing a transcript of an AI agent's working session so the agent can keep working with reduced context. Preserve: (1) the user's original goal, (2) facts the agent has gathered from tool calls (URLs visited, data retrieved, decisions made), (3) any errors or dead-ends to avoid repeating, (4) the agent's current plan or next step. Omit: chain-of-thought reasoning, redundant tool result text, formatting boilerplate. Output ONLY the summary, no preamble.".to_string(),
            },
            ChatMessage {
                role: Role::User,
                content: format!(
                    "Transcript to summarize:\n\n{}\n\nProduce the summary now.",
                    transcript
                ),
            },
        ];

        let summary = self
            .llm
            .chat_completion(&summarization_messages)
            .await?;

        let summary_msg = ChatMessage {
            role: Role::User,
            content: format!(
                "<context_summary>\nEarlier turns of this session were compacted to save context. Summary follows:\n\n{}\n</context_summary>",
                summary.trim()
            ),
        };

        let mut out =
            Vec::with_capacity(preserved_head_end + 1 + (messages.len() - keep_start));
        out.extend_from_slice(&messages[..preserved_head_end]);
        out.push(summary_msg);
        out.extend_from_slice(&messages[keep_start..]);
        Ok(out)
    }

    fn build_system_prompt(&self, skill: &Skill, identity: &str, user_profile: &str, persistent_context: &str) -> String {
        let tool_descriptions = self.tools.get_descriptions(&skill.tools);

        let mut prompt = String::new();

        // SOUL
        prompt.push_str(&self.soul);
        prompt.push_str("\n\n");

        // IDENTITY
        if !identity.is_empty() {
            prompt.push_str(identity);
            prompt.push_str("\n\n");
        }

        // USER
        if !user_profile.is_empty() {
            prompt.push_str(user_profile);
            prompt.push_str("\n\n");
        }

        // Persistent context (memories + recent interactions from storage)
        if !persistent_context.is_empty() {
            prompt.push_str(persistent_context);
            prompt.push('\n');
        }

        // Skill instructions
        prompt.push_str("# Current Task\n\n");
        prompt.push_str(&skill.prompt);
        prompt.push_str("\n\n");

        // Tool descriptions and usage instructions
        if !tool_descriptions.is_empty() {
            prompt.push_str("# Available Tools\n\n");
            for (name, desc) in &tool_descriptions {
                prompt.push_str(&format!("## {}\n{}\n\n", name, desc));
            }

            prompt.push_str("# How to Use Tools\n\n");
            prompt.push_str(
                "To call a tool, include a tool_call block in your response like this:\n\n",
            );
            prompt.push_str("<tool_call>\n");
            prompt.push_str("{\"function\": \"tool_name\", \"parameters\": {}}\n");
            prompt.push_str("</tool_call>\n\n");
            prompt.push_str("The system will execute the tool and return the result in a <tool_result> block.\n");
            prompt.push_str("You may call tools multiple times. When you have all the information you need, respond with your final answer without any <tool_call> blocks.\n\n");
            prompt.push_str("## Act, do not announce\n\n");
            prompt.push_str("If you intend to use a tool, emit the <tool_call> block in the SAME response. Do not say \"I will now do X\" or \"let me check Y\" without including the corresponding <tool_call> in that turn — if you only describe the action, the loop ends and the action never happens. Either:\n");
            prompt.push_str("- Take the action: emit the <tool_call> block (with or without brief prose), or\n");
            prompt.push_str("- Decline / finish: state your final answer with no <tool_call>.\n\n");
            prompt.push_str("Never end a turn with an unfulfilled commitment like \"next, I'll...\" — finish the work in this turn or in a subsequent turn that actually contains the call.\n\n");
        }

        // Memory instructions
        prompt.push_str("# Memory\n\n");
        prompt.push_str("When the user tells you something worth remembering across conversations — preferences, names, important facts, standing instructions — save it with a <memory> tag:\n\n");
        prompt.push_str("<memory>The user's name is Alex</memory>\n\n");
        prompt.push_str("Only save genuinely important, durable facts. Don't save transient details or task-specific information.\n");
        prompt.push_str("Your saved memories persist across all interfaces (TUI, Telegram, etc.) and all future interactions.\n");

        prompt
    }
}

/// Rough token estimate from char count. Real tokenization would round-trip
/// through the model's tokenizer; this is a guardrail, not a billing oracle,
/// so chars/4 (the standard English heuristic) is plenty.
fn estimate_tokens(messages: &[ChatMessage]) -> usize {
    messages.iter().map(|m| m.content.len()).sum::<usize>() / 4
}

// --- Tool call parser ---

#[derive(Debug)]
struct ToolCall {
    name: String,
    params: serde_json::Value,
    /// Set when the call's JSON required repair to parse. Prepended to the
    /// tool_result so the model learns what was wrong and can fix next time.
    repair_note: Option<String>,
}

#[derive(Debug)]
struct ParseError {
    raw: String,
    detail: String,
}

#[derive(Debug, Default)]
struct ParseOutcome {
    calls: Vec<ToolCall>,
    errors: Vec<ParseError>,
}

/// JSON permits these chars after a backslash. Anything else is an invalid
/// escape and the model almost certainly meant the literal char (e.g. `\>`
/// for `>`). See strip_invalid_json_escapes.
fn is_valid_json_escape_char(c: char) -> bool {
    matches!(c, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u')
}

/// Walk the string and drop `\` that precedes a non-escape char. Safe to run
/// on whole JSON blobs: legal backslashes only appear inside string literals,
/// and elsewhere a stray `\` was already a parse error.
fn strip_invalid_json_escapes(s: &str) -> (String, usize) {
    let mut out = String::with_capacity(s.len());
    let mut stripped = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek().copied() {
                Some(next) if is_valid_json_escape_char(next) => {
                    out.push(c);
                    out.push(next);
                    chars.next();
                }
                Some(next) => {
                    out.push(next);
                    chars.next();
                    stripped += 1;
                }
                None => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    (out, stripped)
}

fn parse_tool_calls(text: &str) -> ParseOutcome {
    let mut outcome = ParseOutcome::default();
    let mut search_from = 0;

    while let Some(rel_start) = text[search_from..].find("<tool_call>") {
        let start = search_from + rel_start;
        let after_tag = start + "<tool_call>".len();

        if let Some(rel_end) = text[after_tag..].find("</tool_call>") {
            let inner = text[after_tag..after_tag + rel_end].trim();

            // Repair ladder: plain parse, then closing-brace fixes for truncation,
            // then invalid-escape stripping for `\>` / `\<` style mistakes.
            let mut note: Option<String> = None;
            let parsed = serde_json::from_str::<serde_json::Value>(inner)
                .or_else(|_| {
                    let mut fixed = inner.to_string();
                    fixed.push('}');
                    let r = serde_json::from_str::<serde_json::Value>(&fixed);
                    if r.is_ok() {
                        note = Some(
                            "NOTE: your tool_call JSON was missing a closing `}`; I added one. Please emit complete JSON next time."
                                .to_string(),
                        );
                    }
                    r
                })
                .or_else(|_| {
                    let mut fixed = inner.to_string();
                    fixed.push_str("}}");
                    let r = serde_json::from_str::<serde_json::Value>(&fixed);
                    if r.is_ok() {
                        note = Some(
                            "NOTE: your tool_call JSON was missing two closing `}`; I added them. Please emit complete JSON next time."
                                .to_string(),
                        );
                    }
                    r
                })
                .or_else(|_| {
                    let (stripped, count) = strip_invalid_json_escapes(inner);
                    let r = serde_json::from_str::<serde_json::Value>(&stripped);
                    if r.is_ok() && count > 0 {
                        note = Some(format!(
                            "NOTE: your tool_call JSON contained {} invalid escape sequence(s) (e.g. `\\>` or `\\<`). I stripped the stray backslashes and ran the call. In JSON, `<` and `>` are plain characters — do not prefix them with `\\`.",
                            count
                        ));
                    }
                    r
                });

            match parsed {
                Ok(parsed) => {
                    let name = parsed
                        .get("function")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let params = parsed
                        .get("parameters")
                        .cloned()
                        .unwrap_or(serde_json::Value::Object(Default::default()));

                    if !name.is_empty() {
                        outcome.calls.push(ToolCall { name, params, repair_note: note });
                    } else {
                        outcome.errors.push(ParseError {
                            raw: inner.to_string(),
                            detail: "tool_call has no 'function' field".to_string(),
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to parse tool call JSON: {} — raw: {}", e, inner);
                    outcome.errors.push(ParseError {
                        raw: inner.to_string(),
                        detail: e.to_string(),
                    });
                }
            }

            search_from = after_tag + rel_end + "</tool_call>".len();
        } else {
            break;
        }
    }

    outcome
}

/// Build a short snippet of the raw text around a serde_json error's column.
/// Returns the offending region with `>>` markers so the model can see exactly
/// where it went wrong.
fn error_snippet(raw: &str, detail: &str) -> String {
    // Try to pull "column N" from the serde error message.
    let col = detail
        .split("column ")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse::<usize>().ok());
    match col {
        Some(c) if c > 0 && c <= raw.len() => {
            let start = c.saturating_sub(20);
            let end = (c + 20).min(raw.len());
            format!(
                "...{}>>HERE>>{}...",
                &raw[start..c.saturating_sub(1)],
                &raw[c.saturating_sub(1)..end]
            )
        }
        _ => {
            let end = raw.len().min(80);
            format!("{}{}", &raw[..end], if raw.len() > 80 { "..." } else { "" })
        }
    }
}

// --- Memory tag parser ---

fn parse_memories(text: &str) -> Vec<String> {
    let mut memories = vec![];
    let mut search_from = 0;

    while let Some(rel_start) = text[search_from..].find("<memory>") {
        let after_tag = search_from + rel_start + "<memory>".len();

        if let Some(rel_end) = text[after_tag..].find("</memory>") {
            let content = text[after_tag..after_tag + rel_end].trim();
            if !content.is_empty() {
                memories.push(content.to_string());
            }
            search_from = after_tag + rel_end + "</memory>".len();
        } else {
            break;
        }
    }

    memories
}

fn strip_tags(text: &str) -> String {
    let mut result = text.to_string();
    for (open, close) in [("<memory>", "</memory>"), ("<tool_call>", "</tool_call>")] {
        while let Some(start) = result.find(open) {
            if let Some(end_tag) = result[start..].find(close) {
                let end = start + end_tag + close.len();
                result = format!("{}{}", &result[..start], &result[end..]);
            } else {
                break;
            }
        }
    }
    // Clean up extra blank lines left behind
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_tool_calls_none() {
        let text = "Here is your answer. No tools needed.";
        let outcome = parse_tool_calls(text);
        assert!(outcome.calls.is_empty());
        assert!(outcome.errors.is_empty());
    }

    #[test]
    fn test_parse_single_tool_call() {
        let text = r#"Let me check the time.

<tool_call>
{"function": "datetime", "parameters": {}}
</tool_call>"#;
        let outcome = parse_tool_calls(text);
        assert_eq!(outcome.calls.len(), 1);
        assert_eq!(outcome.calls[0].name, "datetime");
        assert!(outcome.calls[0].repair_note.is_none());
    }

    #[test]
    fn test_parse_multiple_tool_calls() {
        let text = r#"I need two things.

<tool_call>
{"function": "datetime", "parameters": {}}
</tool_call>

<tool_call>
{"function": "weather", "parameters": {"zip": "10001"}}
</tool_call>"#;
        let outcome = parse_tool_calls(text);
        assert_eq!(outcome.calls.len(), 2);
        assert_eq!(outcome.calls[0].name, "datetime");
        assert_eq!(outcome.calls[1].name, "weather");
    }

    #[test]
    fn test_repairs_invalid_escape_in_html() {
        // The real-world bug: model emits `<h2\>` inside a JSON string.
        let text = "<tool_call>\n{\"function\": \"send_email\", \"parameters\": {\"body\": \"<h2\\>hi</h2>\"}}\n</tool_call>";
        let outcome = parse_tool_calls(text);
        assert_eq!(outcome.calls.len(), 1);
        assert_eq!(outcome.calls[0].name, "send_email");
        let note = outcome.calls[0].repair_note.as_deref().unwrap_or("");
        assert!(note.contains("invalid escape"), "note was: {}", note);
        assert_eq!(
            outcome.calls[0].params.get("body").and_then(|v| v.as_str()),
            Some("<h2>hi</h2>")
        );
    }

    #[test]
    fn test_parse_error_reported_when_unrepairable() {
        let text = "<tool_call>\n{this is not json at all\n</tool_call>";
        let outcome = parse_tool_calls(text);
        assert!(outcome.calls.is_empty());
        assert_eq!(outcome.errors.len(), 1);
        assert!(!outcome.errors[0].detail.is_empty());
    }

    #[test]
    fn test_strip_invalid_json_escapes() {
        let (out, count) = strip_invalid_json_escapes(r#"<h2\>hi\n</h2>"#);
        assert_eq!(out, r#"<h2>hi\n</h2>"#);
        assert_eq!(count, 1); // `\>` stripped, `\n` kept (valid escape)
    }

    #[test]
    fn test_error_snippet_marks_column() {
        let raw = "abcdefghijklmnopqrstuvwxyzABCDEFG";
        let snippet = error_snippet(raw, "something at line 1 column 10 blah");
        assert!(snippet.contains(">>HERE>>"));
    }

    #[test]
    fn test_parse_memories() {
        let text =
            "Sure, I'll remember that.\n\n<memory>Safety word is banana</memory>\n\nAnything else?";
        let memories = parse_memories(text);
        assert_eq!(memories, vec!["Safety word is banana"]);
    }

    #[test]
    fn test_parse_multiple_memories() {
        let text =
            "<memory>User's name is Alex</memory>\n<memory>Prefers dark mode</memory>\nGot it!";
        let memories = parse_memories(text);
        assert_eq!(memories.len(), 2);
        assert_eq!(memories[0], "User's name is Alex");
        assert_eq!(memories[1], "Prefers dark mode");
    }

    #[test]
    fn test_strip_tags() {
        let text =
            "Sure, I'll remember that.\n\n<memory>Safety word is banana</memory>\n\nAnything else?";
        let stripped = strip_tags(text);
        assert_eq!(stripped, "Sure, I'll remember that.\n\nAnything else?");
        assert!(!stripped.contains("<memory>"));
    }

    #[test]
    fn test_no_memories() {
        let text = "Just a normal response.";
        assert!(parse_memories(text).is_empty());
        assert_eq!(strip_tags(text), "Just a normal response.");
    }

    #[test]
    fn test_parse_tool_call_missing_one_brace() {
        let text = r#"<tool_call>
{"function": "send_telegram", "parameters": {"message": "hello"}
</tool_call>"#;
        let outcome = parse_tool_calls(text);
        assert_eq!(outcome.calls.len(), 1);
        assert_eq!(outcome.calls[0].name, "send_telegram");
        assert!(
            outcome.calls[0].repair_note.as_deref().unwrap_or("").contains("closing"),
            "expected a closing-brace repair note"
        );
    }

    #[test]
    fn test_parse_tool_call_missing_two_braces() {
        let text = r#"<tool_call>
{"function": "send_telegram", "parameters": {"message": "hello"
</tool_call>"#;
        let outcome = parse_tool_calls(text);
        assert_eq!(outcome.calls.len(), 1);
        assert_eq!(outcome.calls[0].name, "send_telegram");
    }
}
