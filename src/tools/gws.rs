use anyhow::{bail, Result};
use async_trait::async_trait;

use super::Tool;

/// Denied method verbs — these are destructive or send outbound messages.
const DENIED_METHODS: &[&str] = &["delete", "trash", "send", "empty", "remove"];

/// Denied top-level subcommands.
const DENIED_SUBCOMMANDS: &[&str] = &["auth"];

pub struct GwsTool;

/// Split a command string respecting single and double quotes.
fn shell_split(input: &str) -> Result<Vec<String>> {
    let mut args = vec![];
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ' ' | '\t' if !in_single && !in_double => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }

    if in_single || in_double {
        bail!("Unterminated quote in command");
    }

    if !current.is_empty() {
        args.push(current);
    }

    Ok(args)
}

fn is_allowed(args: &[&str]) -> Result<()> {
    if args.is_empty() {
        bail!("No command provided");
    }

    let first = args[0].to_lowercase();
    if DENIED_SUBCOMMANDS.contains(&first.as_str()) {
        bail!("The '{}' subcommand is not allowed", first);
    }

    // Check if any argument matches a denied method verb.
    // gws commands follow `gws <service> <resource> <method>` so the method
    // is typically the 3rd positional arg, but checking all non-flag args is safer.
    for arg in args {
        if arg.starts_with('-') {
            continue;
        }
        let lower = arg.to_lowercase();
        if DENIED_METHODS.contains(&lower.as_str()) {
            bail!(
                "The '{}' method is not allowed — destructive and outbound operations are blocked",
                arg
            );
        }
    }

    Ok(())
}

#[async_trait]
impl Tool for GwsTool {
    fn name(&self) -> &str {
        "gws"
    }

    fn description_for_llm(&self) -> &str {
        "Run a Google Workspace CLI (gws) command. \
         Parameters: {\"command\": \"<args after gws>\"}. \
         API parameters are passed as a JSON string via --params. \
         Examples: {\"command\": \"gmail users messages list --params '{\\\"userId\\\": \\\"me\\\", \\\"maxResults\\\": 5}'\"}, \
         {\"command\": \"drive files list --params '{\\\"q\\\": \\\"name contains report\\\"}'\"}, \
         {\"command\": \"calendar events list --params '{\\\"calendarId\\\": \\\"primary\\\"}'\"}. \
         Supports Drive, Gmail, Calendar, Sheets, Docs, Chat, Admin, and other Workspace APIs. \
         Returns JSON output. Use 'schema <method>' to inspect request/response schemas. \
         Note: destructive operations (delete, trash, send, empty, remove) are blocked."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let command = params
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: command"))?;

        let args = shell_split(command)?;
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        is_allowed(&arg_refs)?;

        let output = tokio::process::Command::new("gws")
            .args(&args)
            .output()
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    anyhow::anyhow!("gws is not installed. Install with: npm install -g @googleworkspace/cli")
                } else {
                    anyhow::anyhow!("Failed to run gws: {}", e)
                }
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            // gws prints structured errors to stdout and info lines (like keyring backend) to stderr.
            // Include both so we always surface the real error.
            let mut error_msg = String::new();
            if !stdout.is_empty() {
                error_msg.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !error_msg.is_empty() {
                    error_msg.push('\n');
                }
                error_msg.push_str(&stderr);
            }
            bail!("gws exited with {}: {}", output.status, error_msg.trim());
        }

        let result = stdout.to_string();

        // Truncate very large output to avoid blowing up context
        const MAX_LEN: usize = 50_000;
        if result.len() > MAX_LEN {
            let mut end = MAX_LEN;
            while !result.is_char_boundary(end) {
                end -= 1;
            }
            Ok(format!(
                "{}\n\n[Truncated — output was {} bytes]",
                &result[..end],
                result.len()
            ))
        } else {
            Ok(result)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allowed_read_commands() {
        assert!(is_allowed(&["drive", "files", "list"]).is_ok());
        assert!(is_allowed(&["gmail", "users", "messages", "get", "--id=abc"]).is_ok());
        assert!(is_allowed(&["calendar", "events", "list"]).is_ok());
        assert!(is_allowed(&["schema", "drive.files.list"]).is_ok());
    }

    #[test]
    fn test_denied_destructive() {
        assert!(is_allowed(&["drive", "files", "delete", "--file-id=abc"]).is_err());
        assert!(is_allowed(&["gmail", "users", "messages", "trash", "--id=abc"]).is_err());
        assert!(is_allowed(&["gmail", "users", "messages", "send"]).is_err());
    }

    #[test]
    fn test_denied_auth() {
        assert!(is_allowed(&["auth", "login"]).is_err());
        assert!(is_allowed(&["auth", "setup"]).is_err());
    }

    #[test]
    fn test_empty_command() {
        assert!(is_allowed(&[]).is_err());
    }

    #[test]
    fn test_shell_split_simple() {
        let args = shell_split("drive files list").unwrap();
        assert_eq!(args, vec!["drive", "files", "list"]);
    }

    #[test]
    fn test_shell_split_single_quotes() {
        let args = shell_split(r#"gmail users messages list --params '{"userId": "me", "maxResults": 5}'"#).unwrap();
        assert_eq!(args, vec![
            "gmail", "users", "messages", "list",
            "--params", r#"{"userId": "me", "maxResults": 5}"#,
        ]);
    }

    #[test]
    fn test_shell_split_double_quotes() {
        let args = shell_split(r#"drive files list --params "some value with spaces""#).unwrap();
        assert_eq!(args, vec![
            "drive", "files", "list",
            "--params", "some value with spaces",
        ]);
    }
}
