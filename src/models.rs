// Serialize/Deserialize intentionally not derived — InboxEntry is manually
// constructed in send_message and manually serialized to JSON in get_inbox
// with different field names (message_id → id, timestamp → created_ts).
// Deriving serde would create a false expectation that serde is used (DR42-H4).
#[derive(Debug, Clone)]
pub struct InboxEntry {
    pub message_id: String,
    pub from_agent: String,
    pub to_recipients: String,
    pub subject: String,
    pub body: String,
    pub timestamp: i64,
    pub project_id: String,
}
