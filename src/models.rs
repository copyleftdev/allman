use serde::{Deserialize, Serialize};

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Agent {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub program: String,
    pub model: String,
    pub inception_ts: i64,
    pub task: String,
    pub last_active_ts: i64,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Message {
    pub id: String,
    pub thread_id: Option<String>,
    pub project_id: String,
    pub from_agent: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_md: String,
    pub created_ts: i64,
    pub importance: String, // "normal", "high", "urgent"
    pub ack_required: bool,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileReservation {
    pub id: String,
    pub project_id: String,
    pub agent_name: String,
    pub path: String,
    pub exclusive: bool,
    pub reason: String,
    pub created_ts: i64,
    pub expires_ts: i64,
    pub released_ts: Option<i64>,
}
