use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use chrono_tz::Tz;

use super::Tool;

pub struct DateTimeTool {
    default_timezone: String,
}

impl DateTimeTool {
    pub fn new(default_timezone: String) -> Self {
        Self { default_timezone }
    }
}

#[async_trait]
impl Tool for DateTimeTool {
    fn name(&self) -> &str {
        "datetime"
    }

    fn description_for_llm(&self) -> &str {
        "Get the current date and time in the user's configured timezone. No parameters needed. Returns the current date, time, timezone, and day of week."
    }

    async fn execute(&self, _params: serde_json::Value) -> Result<String> {
        let tz: Tz = self.default_timezone.parse().unwrap_or(chrono_tz::UTC);
        let now = Utc::now().with_timezone(&tz);
        Ok(now.format("%Y-%m-%d %H:%M:%S %Z (%A)").to_string())
    }
}
