use anyhow::Result;

use crate::llm::{ChatMessage, LlmBackend, Role};
use crate::skills::Skill;
use crate::tools::ToolRegistry;

const MAX_ITERATIONS: usize = 20;

pub struct Agent {
    llm: Box<dyn LlmBackend>,
    tools: ToolRegistry,
    soul: String,
}

impl Agent {
    pub fn new(llm: Box<dyn LlmBackend>, tools: ToolRegistry, soul: String) -> Self {
        Self { llm, tools, soul }
    }

    /// Run the agent loop for a skill with an optional user message.
    /// `history` contains prior conversation messages (for TUI sessions).
    pub async fn run(
        &self,
        skill: &Skill,
        user_message: &str,
        history: &[ChatMessage],
    ) -> Result<String> {
        let system_prompt = self.build_system_prompt(skill);

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
            let tool_calls = parse_tool_calls(&response);

            if tool_calls.is_empty() {
                // No tool calls — this is the final answer
                return Ok(response);
            }

            // Add the assistant's response (with tool call markers) to history
            messages.push(ChatMessage {
                role: Role::Assistant,
                content: response,
            });

            // Execute each tool call and feed results back
            for call in &tool_calls {
                let result = match self
                    .tools
                    .execute(&call.name, call.params.clone(), &skill.tools)
                    .await
                {
                    Ok(output) => output,
                    Err(e) => format!("Error: {}", e),
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

    fn build_system_prompt(&self, skill: &Skill) -> String {
        let tool_descriptions = self.tools.get_descriptions(&skill.tools);

        let mut prompt = String::new();

        // SOUL
        prompt.push_str(&self.soul);
        prompt.push_str("\n\n");

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
            prompt.push_str("{\"tool\": \"tool_name\", \"params\": {}}\n");
            prompt.push_str("</tool_call>\n\n");
            prompt.push_str("The system will execute the tool and return the result in a <tool_result> block.\n");
            prompt.push_str("You may call tools multiple times. When you have all the information you need, respond with your final answer without any <tool_call> blocks.\n");
        }

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

            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(inner) {
                let name = parsed
                    .get("tool")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let params = parsed
                    .get("params")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(Default::default()));

                if !name.is_empty() {
                    calls.push(ToolCall { name, params });
                }
            }

            search_from = after_tag + rel_end + "</tool_call>".len();
        } else {
            break;
        }
    }

    calls
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
{"tool": "datetime", "params": {}}
</tool_call>"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "datetime");
    }

    #[test]
    fn test_parse_multiple_tool_calls() {
        let text = r#"I need two things.

<tool_call>
{"tool": "datetime", "params": {}}
</tool_call>

<tool_call>
{"tool": "weather", "params": {"zip": "10001"}}
</tool_call>"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "datetime");
        assert_eq!(calls[1].name, "weather");
    }
}
