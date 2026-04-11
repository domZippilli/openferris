use anyhow::Result;
use async_trait::async_trait;

use super::Tool;

pub struct JournalLogsTool;

#[async_trait]
impl Tool for JournalLogsTool {
    fn name(&self) -> &str {
        "journal_logs"
    }

    fn description_for_llm(&self) -> &str {
        "View OpenFerris service logs from journalctl. \
         Parameters: {\"lines\": <optional number, default 50>, \"unit\": <optional string, default \"openferris*\">, \"since\": <optional string, e.g. \"1h\", \"30m\", \"today\">}. \
         Returns the most recent log lines for matching systemd units. \
         The 'since' parameter accepts shorthand like '1h' (1 hour ago), '30m' (30 minutes ago), '2d' (2 days ago), or 'today'. \
         Use this to check on service health, debug errors, or review recent activity."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let lines = params
            .get("lines")
            .and_then(|v| v.as_u64())
            .unwrap_or(50);

        let unit = params
            .get("unit")
            .and_then(|v| v.as_str())
            .unwrap_or("openferris*");

        // Validate unit pattern to prevent command injection
        if !unit
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '*' || c == '.')
        {
            anyhow::bail!("Invalid unit pattern: only alphanumerics, hyphens, underscores, dots, and * are allowed");
        }

        let mut cmd = tokio::process::Command::new("journalctl");
        cmd.arg("--user")
            .arg("--no-pager")
            .arg("-u")
            .arg(unit)
            .arg("-n")
            .arg(lines.to_string());

        if let Some(since) = params.get("since").and_then(|v| v.as_str()) {
            let since_value = parse_since(since)?;
            cmd.arg("--since").arg(since_value);
        }

        let output = cmd.output().await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            anyhow::bail!("journalctl failed: {}", stderr.trim());
        }

        if stdout.trim().is_empty() {
            Ok("No log entries found matching the criteria.".to_string())
        } else {
            Ok(stdout.into_owned())
        }
    }
}

/// Parse shorthand time expressions into journalctl --since format.
fn parse_since(s: &str) -> Result<String> {
    if s == "today" {
        return Ok("today".to_string());
    }
    if s == "yesterday" {
        return Ok("yesterday".to_string());
    }

    // Parse shorthand like "1h", "30m", "2d"
    if let Some(num) = s.strip_suffix('m') {
        let n: u64 = num
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid since value: {}", s))?;
        Ok(format!("{} minutes ago", n))
    } else if let Some(num) = s.strip_suffix('h') {
        let n: u64 = num
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid since value: {}", s))?;
        Ok(format!("{} hours ago", n))
    } else if let Some(num) = s.strip_suffix('d') {
        let n: u64 = num
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid since value: {}", s))?;
        Ok(format!("{} days ago", n))
    } else {
        // Pass through as-is (e.g. "2026-04-05 10:00:00")
        // Validate no shell metacharacters
        if s.chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == ':' || c == ' ' || c == '.')
        {
            Ok(s.to_string())
        } else {
            anyhow::bail!(
                "Invalid since value: '{}'. Use shorthand like '1h', '30m', '2d', 'today', or a datetime string.",
                s
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_since_shorthand() {
        assert_eq!(parse_since("30m").unwrap(), "30 minutes ago");
        assert_eq!(parse_since("2h").unwrap(), "2 hours ago");
        assert_eq!(parse_since("7d").unwrap(), "7 days ago");
        assert_eq!(parse_since("today").unwrap(), "today");
        assert_eq!(parse_since("yesterday").unwrap(), "yesterday");
    }

    #[test]
    fn parse_since_datetime_passthrough() {
        assert_eq!(
            parse_since("2026-04-05 10:00:00").unwrap(),
            "2026-04-05 10:00:00"
        );
    }

    #[test]
    fn parse_since_rejects_injection() {
        assert!(parse_since("1h; rm -rf /").is_err());
        assert!(parse_since("$(whoami)").is_err());
    }

    #[test]
    fn parse_since_rejects_bad_number() {
        assert!(parse_since("abch").is_err());
        assert!(parse_since("m").is_err());
    }
}
