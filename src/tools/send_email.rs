use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;

use super::{Tool, require_str};

pub struct SendEmailTool {
    db_path: PathBuf,
    allowed_senders: Vec<String>,
    always_cc: Option<String>,
    owner_emails: Vec<String>,
}

impl SendEmailTool {
    pub fn new(
        db_path: PathBuf,
        allowed_senders: Vec<String>,
        always_cc: Option<String>,
        owner_emails: Vec<String>,
    ) -> Self {
        Self {
            db_path,
            allowed_senders,
            always_cc,
            owner_emails,
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
         Parameters: {\"to\": \"<email address>\", \"subject\": \"<subject line>\", \"body\": \"<email body text>\", \"cc\": \"<optional cc address>\", \"content_type\": \"<optional: text/plain or text/html>\"}. \
         The recipient (and any cc address you supply) must be in the allowed contacts list or someone you have previously emailed. \
         Use this for sending notifications, briefings, or replies to known contacts. \
         Set content_type to text/html when sending HTML-formatted emails."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let to = require_str(&params, "to")?;

        let subject = require_str(&params, "subject")?;

        let body = require_str(&params, "body")?;

        let param_cc = params.get("cc").and_then(|v| v.as_str());
        let content_type = params.get("content_type").and_then(|v| v.as_str());

        // `param_cc` is model-supplied and must be authorized against the
        // allowlist/known-contacts just like `to`. `always_cc` is
        // config-sourced (the user configured it deliberately) and is
        // exempt from that check.
        crate::email::send_email_with_db(
            &self.db_path,
            &self.allowed_senders,
            &self.owner_emails,
            crate::email::OutboundEmail {
                to,
                vetted_cc: self.always_cc.as_deref(),
                unvetted_cc: param_cc,
                subject,
                body,
                in_reply_to: None,
                references: None,
                thread_id: None,
                content_type,
            },
        )
        .await?;

        Ok("Email sent.".to_string())
    }
}
