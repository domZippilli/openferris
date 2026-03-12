use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;

use super::Tool;

pub struct SendEmailTool {
    db_path: PathBuf,
    allowed_senders: Vec<String>,
}

impl SendEmailTool {
    pub fn new(db_path: PathBuf, allowed_senders: Vec<String>) -> Self {
        Self {
            db_path,
            allowed_senders,
        }
    }
}

#[async_trait]
impl Tool for SendEmailTool {
    fn name(&self) -> &str {
        "send_email"
    }

    fn description_for_llm(&self) -> &str {
        "Send an email via Gmail. \
         Parameters: {\"to\": \"<email address>\", \"subject\": \"<subject line>\", \"body\": \"<email body text>\"}. \
         The recipient must be in the allowed contacts list or someone you have previously emailed. \
         Use this for sending notifications, briefings, or replies to known contacts."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let to = params
            .get("to")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: to"))?;

        let subject = params
            .get("subject")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: subject"))?;

        let body = params
            .get("body")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: body"))?;

        crate::email::send_email_with_db(
            &self.db_path,
            &self.allowed_senders,
            to,
            subject,
            body,
        )
        .await?;

        Ok("Email sent.".to_string())
    }
}
