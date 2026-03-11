pub mod datetime;

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;

use crate::config::AppConfig;

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
    }
}
