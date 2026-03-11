use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonRequest {
    pub id: String,
    pub kind: RequestKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RequestKind {
    RunSkill { skill_name: String },
    FreeformMessage { text: String },
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
