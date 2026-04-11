pub mod ask_claude;
pub mod datetime;
pub mod files;
pub mod gws;
pub mod logs;
pub mod run_skill;
pub mod schedule;
pub mod send_email;
pub mod telegram;
pub mod web;

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use crate::config::{self, AppConfig};

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description_for_llm(&self) -> &str;
    async fn execute(&self, params: serde_json::Value) -> Result<String>;
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
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
        self.tools
            .values()
            .filter(|t| allowed.contains(&t.name().to_string()))
            .map(|t| (t.name(), t.description_for_llm()))
            .collect()
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
        self.register(Box::new(files::ListDirTool::new(allowed_dirs)));
        self.register(Box::new(web::FetchUrlTool));
        self.register(Box::new(schedule::ScheduleTool));
        self.register(Box::new(gws::GwsTool));
        self.register(Box::new(logs::JournalLogsTool));
        self.register(Box::new(ask_claude::AskClaudeTool));

        if let Some(ref tg) = config.telegram {
            self.register(Box::new(telegram::SendTelegramTool::new(
                tg.bot_token.clone(),
                tg.default_chat_id,
            )));
        }
    }

    /// Register tools that need access to the database.
    pub fn register_db_tools(&mut self, db_path: std::path::PathBuf, config: &AppConfig) {
        if let Some(ref gmail) = config.gmail {
            self.register(Box::new(send_email::SendEmailTool::new(
                db_path,
                gmail.allowed_senders.clone(),
                gmail.always_cc.clone(),
            )));
        }
    }
}
