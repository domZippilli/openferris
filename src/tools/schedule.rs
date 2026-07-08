use anyhow::Result;
use async_trait::async_trait;

use super::{Tool, require_str};

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
        let action = require_str(&params, "action")?;

        match action {
            "add" => {
                let skill_name = require_str(&params, "skill_name")?;
                let cron_expr = require_str(&params, "cron_expr")?;
                crate::schedule::add_async(skill_name, cron_expr).await
            }
            "remove" => {
                let skill_name = require_str(&params, "skill_name")?;
                crate::schedule::remove_async(skill_name).await
            }
            "list" => crate::schedule::list_async().await,
            other => anyhow::bail!("Unknown action '{}'. Use: add, remove, or list", other),
        }
    }
}
