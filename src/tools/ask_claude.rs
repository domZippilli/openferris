use anyhow::{Context, Result};
use async_trait::async_trait;

use super::Tool;

pub struct AskClaudeTool;

#[async_trait]
impl Tool for AskClaudeTool {
    fn name(&self) -> &str {
        "ask_claude"
    }

    fn description_for_llm(&self) -> &str {
        "Ask Claude Code for help with a question or problem. \
         Parameters: {\"prompt\": \"<your question>\"}. \
         Use this when you need help with: writing or debugging skills, \
         complex data formatting, code generation, or figuring out how to use an API. \
         Returns Claude's response as text."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let prompt = params
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: prompt"))?;

        tracing::info!("ask_claude: sending prompt ({} chars)", prompt.len());

        let output = tokio::process::Command::new("claude")
            .args(["-p", prompt])
            .output()
            .await
            .context("Failed to run claude CLI")?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            anyhow::bail!("claude exited with {}: {}{}", output.status, stdout, stderr);
        }

        let response = stdout.trim().to_string();
        tracing::info!("ask_claude: got response ({} chars)", response.len());

        Ok(response)
    }
}
