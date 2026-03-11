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
}
