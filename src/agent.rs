use anyhow::Result;

use crate::llm::{ChatMessage, LlmBackend, Role};
use crate::skills::Skill;
use crate::tools::ToolRegistry;

const MAX_ITERATIONS: usize = 50;

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
    ) -> Result<AgentResult> {
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

        for iteration in 0..MAX_ITERATIONS {
            tracing::debug!("Agent iteration {}", iteration + 1);

            let response = self.llm.chat_completion(&messages).await?;
            tracing::debug!("LLM response: {}", response);
            let tool_calls = parse_tool_calls(&response);

            if tool_calls.is_empty() {
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
            for call in &tool_calls {
                tracing::info!("Tool call: {}", call.name);
                tracing::debug!("Tool params: {} {}", call.name, call.params);
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

                messages.push(ChatMessage {
                    role: Role::User,
                    content: format!(
                        "<tool_result tool=\"{}\">\n{}\n</tool_result>",
                        call.name, result
                    ),
                });
            }
        }

        anyhow::bail!("Agent exceeded maximum iterations ({})", MAX_ITERATIONS)
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

// --- Tool call parser ---

#[derive(Debug)]
struct ToolCall {
    name: String,
    params: serde_json::Value,
}

fn parse_tool_calls(text: &str) -> Vec<ToolCall> {
    let mut calls = vec![];
    let mut search_from = 0;

    while let Some(rel_start) = text[search_from..].find("<tool_call>") {
        let start = search_from + rel_start;
        let after_tag = start + "<tool_call>".len();

        if let Some(rel_end) = text[after_tag..].find("</tool_call>") {
            let inner = text[after_tag..after_tag + rel_end].trim();

            // Try parsing as-is first, then attempt to repair truncated JSON
            // by appending closing braces. LLMs sometimes drop trailing `}`.
            let parsed = serde_json::from_str::<serde_json::Value>(inner)
                .or_else(|_| {
                    let mut fixed = inner.to_string();
                    fixed.push('}');
                    serde_json::from_str::<serde_json::Value>(&fixed)
                })
                .or_else(|_| {
                    let mut fixed = inner.to_string();
                    fixed.push_str("}}");
                    serde_json::from_str::<serde_json::Value>(&fixed)
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
                        calls.push(ToolCall { name, params });
                    } else {
                        tracing::warn!("Tool call block has no 'function' field: {}", inner);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to parse tool call JSON: {} — raw: {}", e, inner);
                }
            }

            search_from = after_tag + rel_end + "</tool_call>".len();
        } else {
            break;
        }
    }

    calls
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
        assert!(parse_tool_calls(text).is_empty());
    }

    #[test]
    fn test_parse_single_tool_call() {
        let text = r#"Let me check the time.

<tool_call>
{"function": "datetime", "parameters": {}}
</tool_call>"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "datetime");
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
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "datetime");
        assert_eq!(calls[1].name, "weather");
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
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "send_telegram");
    }

    #[test]
    fn test_parse_tool_call_missing_two_braces() {
        let text = r#"<tool_call>
{"function": "send_telegram", "parameters": {"message": "hello"
</tool_call>"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "send_telegram");
    }
}
