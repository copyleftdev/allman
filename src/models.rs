use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxEntry {
    pub message_id: String,
    pub from_agent: String,
    pub subject: String,
    pub body: String,
    pub timestamp: i64,
    pub project_id: String,
}
