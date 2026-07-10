pub mod ask_claude;
pub mod ask_codex;
pub mod datetime;
pub mod files;
pub mod gws;
pub mod logs;
pub mod ocr;
pub mod run_skill;
pub mod schedule;
pub mod scrape;
pub mod search;
pub mod send_email;
pub mod stealth;
pub mod telegram;
pub mod wakeup;
pub mod web;

use crate::config::{self, AppConfig};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;

/// Extract a required string parameter `key` from a tool's `params`, or fail
/// with the standard "Missing required parameter: {key}" error every tool
/// used to spell out by hand.
pub fn require_str<'a>(params: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: {}", key))
}

/// Truncate `s` to at most `max_bytes` bytes (snapped back to the nearest
/// char boundary so multi-byte UTF-8 is never split), appending a note that
/// names `label` and reports the original and kept sizes. Used by tools that
/// cap large output/response/document text before it goes back to the model,
/// to avoid blowing up context. A no-op (returns `s` unchanged) when `s` is
/// already within budget.
pub fn truncate_for_context(mut s: String, max_bytes: usize, label: &str) -> String {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let original_len = s.len();
    s.truncate(end);
    s.push_str(&format!(
        "\n\n[{} was {} bytes, truncated to {}]",
        label, original_len, end
    ));
    s
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description_for_llm(&self) -> &str;
    async fn execute(&self, params: serde_json::Value) -> Result<String>;
    /// Called by the agent at the start of every Agent::run(). Tools that
    /// hold per-run state (e.g. a session id for multi-turn ask_claude/ask_codex)
    /// reset it here. Default: no-op.
    fn on_run_start(&self) {}
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name, tool);
    }

    /// Get tool descriptions filtered by the skill's allowlist (tool sieve)
    pub fn get_descriptions(&self, allowed: &[String]) -> Vec<(&str, &str)> {
        for name in allowed {
            if !self.tools.contains_key(name) {
                tracing::warn!(
                    "Skill allowlist references unknown tool '{}' — it will be silently dropped \
                     (check for typos or workspace-skill names that aren't registered tools)",
                    name
                );
            }
        }

        let mut descriptions: Vec<(&str, &str)> = self
            .tools
            .values()
            .filter(|t| allowed.contains(&t.name().to_string()))
            .map(|t| (t.name(), t.description_for_llm()))
            .collect();
        descriptions.sort_by_key(|(name, _)| *name);
        descriptions
    }

    /// Notify all tools that a new agent run is starting. Tools holding
    /// per-run state reset it here.
    pub fn notify_run_start(&self) {
        for tool in self.tools.values() {
            tool.on_run_start();
        }
    }

    /// Execute a tool, enforcing the sieve
    pub async fn execute(
        &self,
        name: &str,
        params: serde_json::Value,
        allowed: &[String],
    ) -> Result<String> {
        if !allowed.contains(&name.to_string()) {
            anyhow::bail!("Tool '{}' is not allowed by this skill", name);
        }

        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Unknown tool: {}", name))?;

        tool.execute(params).await
    }

    /// Register all built-in tools
    pub fn register_defaults(&mut self, config: &AppConfig) {
        self.register(Box::new(datetime::DateTimeTool::new(
            config.user.timezone.clone(),
        )));

        let allowed_dirs = config::allowed_directories(&config.files);
        self.register(Box::new(files::ReadFileTool::new(allowed_dirs.clone())));
        self.register(Box::new(files::WriteFileTool::new(allowed_dirs.clone())));
        self.register(Box::new(files::ListDirTool::new(allowed_dirs.clone())));
        self.register(Box::new(ocr::OcrImageTool::new(allowed_dirs.clone())));
        self.register(Box::new(web::FetchUrlTool::new(
            config.fetch.allowed_local_ports.clone(),
        )));
        self.register(Box::new(schedule::ScheduleTool));
        self.register(Box::new(gws::GwsTool::new(config.gws.clone())));
        self.register(Box::new(gws::GwsCalendarListEventsTool));
        self.register(Box::new(gws::GwsCalendarGetEventTool));
        self.register(Box::new(gws::GwsDriveDownloadFileTool));
        self.register(Box::new(gws::GwsDriveDownloadFileToPathTool::new(
            allowed_dirs.clone(),
        )));
        self.register(Box::new(logs::JournalLogsTool));
        self.register(Box::new(ask_claude::AskClaudeTool::new()));
        self.register(Box::new(ask_codex::AskCodexTool::new()));

        if let Some(ref s) = config.search {
            self.register(Box::new(search::WebSearchTool::new(s.endpoint.clone())));
        }

        if let Some(ref f) = config.firecrawl {
            self.register(Box::new(scrape::ScrapeUrlTool::new(f.endpoint.clone())));
        }

        if let Some(ref c) = config.camoufox {
            self.register(Box::new(stealth::StealthFetchTool::new(c.endpoint.clone())));
        }

        if let Some(ref tg) = config.telegram {
            self.register(Box::new(telegram::SendTelegramTool::new(
                tg.bot_token.clone(),
                tg.default_chat_id,
                tg.allowed_users.clone(),
            )));
        }
    }

    /// Register tools that need access to the database.
    pub fn register_db_tools(&mut self, db_path: std::path::PathBuf, config: &AppConfig) {
        // Unconditional: set_wakeup needs Storage but no external service
        // config, unlike the Telegram/Gmail delivery tools below.
        self.register(Box::new(wakeup::SetWakeupTool::new(
            db_path.clone(),
            config.user.timezone.clone(),
        )));

        if let Some(ref tg) = config.telegram {
            self.register(Box::new(telegram::SendTelegramTool::new_with_storage(
                tg.bot_token.clone(),
                tg.default_chat_id,
                tg.allowed_users.clone(),
                db_path.clone(),
            )));
        }

        if let Some(ref gmail) = config.gmail {
            self.register(Box::new(send_email::SendEmailTool::new(
                db_path,
                gmail.allowed_senders.clone(),
                gmail.always_cc.clone(),
                config.user.emails.clone(),
            )));
        }
    }
}

#[cfg(test)]
mod helper_tests {
    use super::*;

    #[test]
    fn require_str_returns_value_when_present() {
        let params = serde_json::json!({"prompt": "hello"});
        assert_eq!(require_str(&params, "prompt").unwrap(), "hello");
    }

    #[test]
    fn require_str_errors_when_missing() {
        let params = serde_json::json!({});
        let err = require_str(&params, "prompt").unwrap_err();
        assert_eq!(err.to_string(), "Missing required parameter: prompt");
    }

    #[test]
    fn require_str_errors_when_wrong_type() {
        let params = serde_json::json!({"prompt": 5});
        assert!(require_str(&params, "prompt").is_err());
    }

    #[test]
    fn truncate_for_context_passthrough_when_within_budget() {
        let s = "hello".to_string();
        assert_eq!(truncate_for_context(s.clone(), 100, "output"), s);
    }

    #[test]
    fn truncate_for_context_truncates_and_notes_sizes() {
        let s = "a".repeat(100);
        let result = truncate_for_context(s, 10, "output");
        assert!(result.starts_with(&"a".repeat(10)));
        assert!(result.contains("[output was 100 bytes, truncated to 10]"));
    }

    #[test]
    fn truncate_for_context_snaps_to_char_boundary() {
        // Each 'é' is 2 bytes; a max_bytes of 5 lands mid-char at byte 5.
        let s = "ééééé".to_string(); // 10 bytes, 5 chars
        let result = truncate_for_context(s, 5, "output");
        // Should back off to the nearest char boundary (byte 4 = 2 chars).
        assert!(result.contains("truncated to 4"));
    }
}
