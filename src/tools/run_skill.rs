use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use super::Tool;
use crate::agent::Agent;
use crate::config::LlmConfig;
use crate::llm::llamacpp::LlamaCppBackend;
use crate::skills;
use crate::tools::ToolRegistry;

pub struct RunSkillTool {
    llm_config: LlmConfig,
    app_config: crate::config::AppConfig,
    soul: String,
    identity: String,
    user_profile: String,
    skills_dir: PathBuf,
    db_path: PathBuf,
}

impl RunSkillTool {
    pub fn new(
        llm_config: LlmConfig,
        app_config: crate::config::AppConfig,
        soul: String,
        identity: String,
        user_profile: String,
        skills_dir: PathBuf,
        db_path: PathBuf,
    ) -> Self {
        Self {
            llm_config,
            app_config,
            soul,
            identity,
            user_profile,
            skills_dir,
            db_path,
        }
    }
}

#[async_trait]
impl Tool for RunSkillTool {
    fn name(&self) -> &str {
        "run_skill"
    }

    fn description_for_llm(&self) -> &str {
        "Run a skill as a subagent. The skill runs with its own context and tools, and returns the result. \
         Parameters: {\"skill_name\": \"<name>\", \"context\": \"<optional extra instructions or data>\"}. \
         Use this to delegate tasks to specialized skills like headline-scrape, daily-briefing, etc. \
         The subagent has access to all the same tools (except run_skill) and will execute the skill's instructions."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let skill_name = params
            .get("skill_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: skill_name"))?;

        let context = params
            .get("context")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        tracing::info!("Subagent starting: skill={}", skill_name);

        // Load the skill
        let skill = skills::load_skill(skill_name, &self.skills_dir)?;

        // Create LLM backend on slot 1 (parent uses slot 0)
        let llm: Box<dyn crate::llm::LlmBackend> = Box::new(LlamaCppBackend::new(
            self.llm_config.endpoint.clone(),
            self.llm_config.model.clone(),
            1,
        ));

        // Build tool registry WITHOUT run_skill to prevent recursion
        let mut tools = ToolRegistry::new();
        tools.register_defaults(&self.app_config);
        tools.register_db_tools(self.db_path.clone(), &self.app_config);

        let agent = Agent::new(llm, tools, self.soul.clone());

        let msg = match &context {
            Some(ctx) => format!("Execute the {} skill now.\n\n{}", skill_name, ctx),
            None => format!("Execute the {} skill now.", skill_name),
        };

        let result = agent
            .run(
                &skill,
                &msg,
                &[],
                &self.identity,
                &self.user_profile,
                "",
            )
            .await?;

        tracing::info!("Subagent finished: skill={} ({} bytes)", skill_name, result.response.len());

        Ok(result.response)
    }
}
