use std::sync::Mutex;

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::Tool;

pub struct AskClaudeTool {
    /// Session id from the previous ask_claude call in the current agent run,
    /// or None if this is the first call. Cleared on Agent::run() start via
    /// `on_run_start`. The next call resumes the same Claude conversation, so
    /// the model can have a multi-turn dialogue without managing the id.
    session: Mutex<Option<String>>,
}

impl AskClaudeTool {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
        }
    }
}

#[async_trait]
impl Tool for AskClaudeTool {
    fn name(&self) -> &str {
        "ask_claude"
    }

    fn description_for_llm(&self) -> &str {
        "Ask Claude Code for help with a question or problem. \
         Parameters: {\"prompt\": \"<your question>\"}. \
         Returns Claude's response as text. \
         Subsequent calls within the SAME skill run continue the same conversation, \
         so you can ask follow-ups (\"can you go deeper on point 2?\", \"what about edge case X?\") \
         without re-explaining context. The conversation resets when the skill run ends. \
         Use for: writing or debugging skills, complex data formatting, code generation, \
         API exploration, or sanity-checking your own diagnoses."
    }

    fn on_run_start(&self) {
        *self.session.lock().expect("session mutex poisoned") = None;
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let prompt = params
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: prompt"))?;

        let resume = self
            .session
            .lock()
            .expect("session mutex poisoned")
            .clone();

        tracing::info!(
            "ask_claude: prompt ({} chars), resuming={}",
            prompt.len(),
            resume.is_some()
        );

        let mut cmd = tokio::process::Command::new("claude");
        cmd.arg("-p").arg(prompt).args(["--output-format", "json"]);
        if let Some(ref id) = resume {
            cmd.args(["--resume", id]);
        }

        let output = cmd.output().await.context("Failed to run claude CLI")?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            anyhow::bail!("claude exited with {}: {}{}", output.status, stdout, stderr);
        }

        let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
            .with_context(|| format!("claude --output-format json returned non-JSON: {}", stdout))?;

        if parsed.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
            anyhow::bail!(
                "claude reported an error: {}",
                parsed
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(no result field)")
            );
        }

        let result = parsed
            .get("result")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("claude JSON missing 'result' field: {}", stdout))?
            .to_string();

        if let Some(new_id) = parsed.get("session_id").and_then(|v| v.as_str()) {
            *self.session.lock().expect("session mutex poisoned") = Some(new_id.to_string());
        }

        tracing::info!("ask_claude: got response ({} chars)", result.len());
        Ok(result)
    }
}
