use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use super::{Tool, require_str};
use crate::agent::Agent;
use crate::config::LlmConfig;
use crate::llm::model_adapter::create_model_adapter;
use crate::llm::openai_compat::OpenAiCompatBackend;
use crate::skills;
use crate::tools::ToolRegistry;

pub struct RunSkillTool {
    llm_config: LlmConfig,
    app_config: crate::config::AppConfig,
    soul: String,
    user_profile: String,
    skills_dir: PathBuf,
    db_path: PathBuf,
}

impl RunSkillTool {
    pub fn new(
        llm_config: LlmConfig,
        app_config: crate::config::AppConfig,
        soul: String,
        user_profile: String,
        skills_dir: PathBuf,
        db_path: PathBuf,
    ) -> Self {
        Self {
            llm_config,
            app_config,
            soul,
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
        "Run a skill as a subagent. The skill runs with its own context and tools, and returns the result as text. \
         Parameters: {\"skill_name\": \"<name>\", \"context\": \"<optional extra instructions or data>\"}. \
         Use this to delegate tasks to specialized skills like daily-briefing, email-reply, etc. \
         The subagent does the work (fetching, parsing, formatting) and returns the result to you. \
         Delivery tools are disabled inside the subagent: run_skill itself never sends email or other external delivery, even if the delegated skill normally would. \
         If the result needs to be delivered, you must explicitly call send_email or another delivery tool yourself after run_skill returns. \
         Do not claim a delegated skill was delivered unless you personally called the delivery tool and it succeeded."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let skill_name = require_str(&params, "skill_name")?;

        let context = params
            .get("context")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        tracing::info!("Subagent starting: skill={}", skill_name);

        // Load the skill and strip delivery tools — the parent handles delivery.
        let mut skill = skills::load_skill(skill_name, &self.skills_dir)?;
        const DELIVERY_TOOLS: &[&str] = &["send_email"];
        skill
            .tools
            .retain(|t| !DELIVERY_TOOLS.contains(&t.as_str()));

        // Create LLM backend on slot 1 (parent uses slot 0)
        let llm: Box<dyn crate::llm::LlmBackend> = Box::new(OpenAiCompatBackend::new(
            self.llm_config.endpoint.clone(),
            self.llm_config.model.clone(),
            self.llm_config.temperature,
            self.llm_config.top_k,
            self.llm_config.enable_thinking,
            1,
            create_model_adapter(&self.llm_config.model_adapter)?,
        )?);

        // Build tool registry WITHOUT run_skill to prevent recursion
        let mut tools = ToolRegistry::new();
        tools.register_defaults(&self.app_config);
        tools.register_db_tools(self.db_path.clone(), &self.app_config);

        let agent = Agent::new(llm, tools, self.soul.clone());

        let base = format!(
            "Execute the {} skill now. \
             Do NOT send or deliver the result — just return the formatted output as your response. \
             The caller will handle delivery.",
            skill_name
        );
        let msg = match &context {
            Some(ctx) => format!("{}\n\n{}", base, ctx),
            None => base,
        };

        let result = agent
            .run(
                &skill,
                &msg,
                &[],
                crate::agent::PromptContext {
                    user_profile: &self.user_profile,
                    persistent_context: "",
                },
                None,
            )
            .await?;

        tracing::info!(
            "Subagent finished: skill={} ({} bytes)",
            skill_name,
            result.response.len()
        );

        Ok(result.response)
    }
}
