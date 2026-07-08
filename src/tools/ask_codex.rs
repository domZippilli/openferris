use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::{Tool, require_str};

const CODEX_TIMEOUT: Duration = Duration::from_secs(15 * 60);

pub struct AskCodexTool {
    /// Thread id from the previous ask_codex call in the current agent run.
    /// Cleared on Agent::run() start via `on_run_start`.
    thread_id: Mutex<Option<String>>,
}

impl Default for AskCodexTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AskCodexTool {
    pub fn new() -> Self {
        Self {
            thread_id: Mutex::new(None),
        }
    }
}

#[derive(Debug, Default)]
struct CodexExecOutput {
    thread_id: Option<String>,
    response: String,
}

fn parse_codex_jsonl(stdout: &str) -> Result<CodexExecOutput> {
    let mut parsed = CodexExecOutput::default();
    let mut messages = Vec::new();

    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let event: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("codex --json returned non-JSONL line: {}", line))?;

        if event.get("type").and_then(|v| v.as_str()) == Some("thread.started")
            && let Some(id) = event.get("thread_id").and_then(|v| v.as_str())
        {
            parsed.thread_id = Some(id.to_string());
        }

        if event.get("type").and_then(|v| v.as_str()) == Some("item.completed") {
            let item = event.get("item").unwrap_or(&serde_json::Value::Null);
            if item.get("type").and_then(|v| v.as_str()) == Some("agent_message")
                && let Some(text) = item.get("text").and_then(|v| v.as_str())
            {
                messages.push(text.to_string());
            }
        }
    }

    parsed.response = messages.join("\n\n");
    Ok(parsed)
}

#[async_trait]
impl Tool for AskCodexTool {
    fn name(&self) -> &str {
        "ask_codex"
    }

    fn description_for_llm(&self) -> &str {
        "Ask Codex for help with a question or problem. \
         Parameters: {\"prompt\": \"<your question>\"}. \
         Runs Codex non-interactively with `codex exec --skip-git-repo-check`. \
         Returns Codex's final response as text. \
         Subsequent calls within the SAME skill run continue the same Codex thread, \
         so you can ask follow-ups without re-explaining context. \
         The conversation resets when the skill run ends. \
         Use for: writing or debugging skills, code review, implementation advice, \
         API exploration, or sanity-checking your own diagnoses."
    }

    fn on_run_start(&self) {
        *self.thread_id.lock().expect("thread_id mutex poisoned") = None;
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let prompt = require_str(&params, "prompt")?;

        let resume = self
            .thread_id
            .lock()
            .expect("thread_id mutex poisoned")
            .clone();

        tracing::info!(
            "ask_codex: prompt ({} chars), resuming={}",
            prompt.len(),
            resume.is_some()
        );

        let mut cmd = tokio::process::Command::new("codex");
        cmd.stdin(Stdio::null());

        if let Some(ref id) = resume {
            cmd.args([
                "exec",
                "resume",
                "--skip-git-repo-check",
                "--json",
                id,
                prompt,
            ]);
        } else {
            cmd.args(["exec", "--skip-git-repo-check", "--json", prompt]);
        }
        cmd.kill_on_drop(true);

        let output = tokio::time::timeout(CODEX_TIMEOUT, cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("codex timed out after {:?}", CODEX_TIMEOUT))?
            .context("Failed to run codex CLI")?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            anyhow::bail!("codex exited with {}: {}{}", output.status, stdout, stderr);
        }

        let parsed = parse_codex_jsonl(&stdout)?;

        if let Some(new_id) = parsed.thread_id {
            *self.thread_id.lock().expect("thread_id mutex poisoned") = Some(new_id);
        }

        if parsed.response.is_empty() {
            anyhow::bail!("codex JSONL did not include an agent message: {}", stdout);
        }

        tracing::info!("ask_codex: got response ({} chars)", parsed.response.len());
        Ok(parsed.response)
    }
}

#[cfg(test)]
mod tests {
    use super::parse_codex_jsonl;

    #[test]
    fn parses_thread_id_and_agent_message() {
        let stdout = r#"{"type":"thread.started","thread_id":"abc-123"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"hello"}}
{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}"#;

        let parsed = parse_codex_jsonl(stdout).unwrap();

        assert_eq!(parsed.thread_id.as_deref(), Some("abc-123"));
        assert_eq!(parsed.response, "hello");
    }
}
