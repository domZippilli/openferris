use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonRequest {
    pub id: String,
    pub kind: RequestKind,
    /// Where this request originated from (e.g., "tui", "cli", "web").
    #[serde(default)]
    pub source: Option<String>,
    /// Stable conversation key. Freeform messages sharing a `session_id` are
    /// threaded together so the agent sees prior turns even across separate
    /// daemon connections (e.g. the web client opens a new connection per message).
    /// `None` means a one-shot request with no conversational continuity.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RequestKind {
    GetHistory {
        channel: String,
        limit: usize,
    },
    RunSkill {
        skill_name: String,
        #[serde(default)]
        context: Option<String>,
    },
    FreeformMessage {
        text: String,
    },
    PursueGoal {
        exit_criteria: String,
        max_turns: usize,
    },
    StoreMemory {
        content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisplayMessage {
    pub role: String,
    pub text: String,
    pub timestamp: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub request_id: String,
    pub kind: ResponseKind,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ResponseKind {
    History {
        messages: Vec<DisplayMessage>,
    },
    Done {
        text: String,
    },
    Error {
        message: String,
    },
    /// Intermediate progress update sent while the agent is working.
    Progress {
        text: String,
    },
    /// A streamed chunk of assistant prose. Multiple of these arrive between
    /// the request and the final `Done`. Clients should append/coalesce.
    AssistantChunk {
        text: String,
    },
}

/// In-process notification carried on the agent→daemon channel. The daemon
/// translates each variant to the appropriate wire `ResponseKind`.
#[derive(Debug, Clone)]
pub enum AgentNotification {
    /// Tool invocation about to start, e.g. "Checking the time...".
    ToolProgress(String),
    /// Streamed text chunk from the LLM (assistant prose, not tool-call markup).
    AssistantChunk(String),
}

/// Parse a `/goal [--max-turns N] <exit criteria>` command body into the
/// `(max_turns, exit_criteria)` pair used by the web and TUI clients.
pub fn parse_goal_args(input: &str) -> Result<(usize, String), String> {
    let mut parts = input.split_whitespace().peekable();
    let mut max_turns = 5usize;
    let mut criteria = Vec::new();

    while let Some(part) = parts.next() {
        if part == "--max-turns" {
            let Some(raw) = parts.next() else {
                return Err("Usage: /goal [--max-turns N] <exit criteria>".to_string());
            };
            max_turns = raw
                .parse::<usize>()
                .map_err(|_| "max turns must be a positive integer".to_string())?;
        } else if let Some(raw) = part.strip_prefix("--max-turns=") {
            max_turns = raw
                .parse::<usize>()
                .map_err(|_| "max turns must be a positive integer".to_string())?;
        } else {
            criteria.push(part);
            criteria.extend(parts);
            break;
        }
    }

    let exit_criteria = criteria.join(" ").trim().to_string();
    if exit_criteria.is_empty() {
        return Err("Usage: /goal [--max-turns N] <exit criteria>".to_string());
    }

    Ok((max_turns, exit_criteria))
}

/// Map internal tool names to human-friendly progress labels.
pub fn tool_progress_label(tool_name: &str) -> &'static str {
    match tool_name {
        "datetime" => "Checking the time...",
        "read_file" => "Reading a file...",
        "write_file" => "Writing a file...",
        "list_dir" => "Browsing files...",
        "ocr_image" => "Reading text from an image...",
        "fetch_url" => "Fetching a web page...",
        "schedule" => "Checking the schedule...",
        "gws" => "Querying Google...",
        "gws.drive.download_file" => "Downloading a Drive file...",
        "gws.drive.download_file_to_path" => "Downloading a Drive file...",
        "journal_logs" => "Reading system logs...",
        "ask_claude" => "Thinking harder...",
        "ask_codex" => "Asking Codex...",
        "send_email" => "Sending an email...",
        "run_skill" => "Running a sub-task...",
        _ => "Working...",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_goal_args_plain_criteria_uses_default_max_turns() {
        let (max_turns, criteria) = parse_goal_args("book a table for Friday").unwrap();
        assert_eq!(max_turns, 5);
        assert_eq!(criteria, "book a table for Friday");
    }

    #[test]
    fn parse_goal_args_accepts_space_separated_max_turns() {
        let (max_turns, criteria) = parse_goal_args("--max-turns 10 find a plumber").unwrap();
        assert_eq!(max_turns, 10);
        assert_eq!(criteria, "find a plumber");
    }

    #[test]
    fn parse_goal_args_accepts_equals_max_turns() {
        let (max_turns, criteria) = parse_goal_args("--max-turns=3 find a plumber").unwrap();
        assert_eq!(max_turns, 3);
        assert_eq!(criteria, "find a plumber");
    }

    #[test]
    fn parse_goal_args_errors_on_empty_criteria() {
        assert!(parse_goal_args("--max-turns 3").is_err());
        assert!(parse_goal_args("").is_err());
    }

    #[test]
    fn parse_goal_args_errors_on_non_numeric_max_turns() {
        assert!(parse_goal_args("--max-turns abc find a plumber").is_err());
        assert!(parse_goal_args("--max-turns=abc find a plumber").is_err());
    }

    #[test]
    fn parse_goal_args_errors_on_missing_max_turns_value() {
        assert!(parse_goal_args("--max-turns").is_err());
    }
}
