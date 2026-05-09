use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonRequest {
    pub id: String,
    pub kind: RequestKind,
    /// Where this request originated from (e.g., "tui", "cli", "telegram").
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RequestKind {
    RunSkill {
        skill_name: String,
        #[serde(default)]
        context: Option<String>,
    },
    FreeformMessage { text: String },
    StoreMemory { content: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub request_id: String,
    pub kind: ResponseKind,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ResponseKind {
    Done { text: String },
    Error { message: String },
    /// Intermediate progress update sent while the agent is working.
    Progress { text: String },
    /// A streamed chunk of assistant prose. Multiple of these arrive between
    /// the request and the final `Done`. Clients should append/coalesce.
    AssistantChunk { text: String },
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

/// Map internal tool names to human-friendly progress labels.
pub fn tool_progress_label(tool_name: &str) -> &'static str {
    match tool_name {
        "datetime" => "Checking the time...",
        "read_file" => "Reading a file...",
        "write_file" => "Writing a file...",
        "list_dir" => "Browsing files...",
        "fetch_url" => "Fetching a web page...",
        "schedule" => "Checking the schedule...",
        "gws" => "Querying Google...",
        "journal_logs" => "Reading system logs...",
        "ask_claude" => "Thinking harder...",
        "send_telegram" => "Sending a Telegram message...",
        "send_email" => "Sending an email...",
        "run_skill" => "Running a sub-task...",
        _ => "Working...",
    }
}
