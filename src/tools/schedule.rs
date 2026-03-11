use anyhow::Result;
use async_trait::async_trait;

use super::Tool;

pub struct ScheduleTool;

#[async_trait]
impl Tool for ScheduleTool {
    fn name(&self) -> &str {
        "schedule"
    }

    fn description_for_llm(&self) -> &str {
        "Manage scheduled skill invocations via cron. \
         Parameters: {\"action\": \"add|remove|list\", \"skill_name\": \"<name>\", \"cron_expr\": \"<cron expression>\"}. \
         For 'add': skill_name and cron_expr are required. Example cron_expr: \"0 7 * * *\" (7am daily), \"*/30 * * * *\" (every 30 min), \"0 9 * * 1-5\" (weekdays at 9am). \
         For 'remove': skill_name is required. \
         For 'list': no other parameters needed."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: action"))?;

        match action {
            "add" => {
                let skill_name = params
                    .get("skill_name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing required parameter: skill_name"))?;
                let cron_expr = params
                    .get("cron_expr")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing required parameter: cron_expr"))?;
                crate::schedule::add(skill_name, cron_expr)
            }
            "remove" => {
                let skill_name = params
                    .get("skill_name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing required parameter: skill_name"))?;
                crate::schedule::remove(skill_name)
            }
            "list" => crate::schedule::list(),
            other => anyhow::bail!("Unknown action '{}'. Use: add, remove, or list", other),
        }
    }
}
