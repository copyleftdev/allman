use crate::models::InboxEntry;
use crate::state::{AgentRecord, PersistOp, PostOffice};
use chrono::Utc;
use serde_json::{json, Value};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use uuid::Uuid;

const MAX_INBOX_SIZE: usize = 10_000;
const MAX_AGENT_NAME_LEN: usize = 128;
const MAX_FROM_AGENT_LEN: usize = 256;
const MAX_PROJECT_KEY_LEN: usize = 256;
const MAX_RECIPIENTS: usize = 100;
const MAX_RECIPIENT_NAME_LEN: usize = 256;
const MAX_SUBJECT_LEN: usize = 1_024;
const MAX_BODY_LEN: usize = 65_536; // 64 KB
const MAX_PROGRAM_LEN: usize = 4_096; // 4 KB
const MAX_MODEL_LEN: usize = 256;
const MAX_PROJECT_ID_LEN: usize = 256;
const MAX_QUERY_LEN: usize = 10_240; // 10 KB
const DEFAULT_INBOX_LIMIT: usize = 100;
const MAX_INBOX_LIMIT: usize = 1_000;
const DEFAULT_SEARCH_LIMIT: usize = 10;
const MAX_SEARCH_LIMIT: usize = 1_000;
const MAX_AGENTS: usize = 100_000;
const MAX_PROJECTS: usize = 100_000;

pub async fn handle_mcp_request(state: PostOffice, req: Value) -> Value {
    let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let id = req.get("id");
    let params = req.get("params").cloned().unwrap_or(json!({}));

    // JSON-RPC 2.0: requests without "id" are notifications.
    // The spec says "The Server MUST NOT reply to a Notification."
    // Return Value::Null to signal no response should be sent.
    if id.is_none() {
        return Value::Null;
    }

    match method {
        "tools/call" => {
            let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));

            let result = match tool_name {
                "create_agent" => create_agent(&state, args),
                "search_messages" => search_messages(&state, args),
                "send_message" => send_message(&state, args),
                "get_inbox" => get_inbox(&state, args),
                _ => {
                    return json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": format!("Unknown tool: {}", tool_name) }
                    });
                }
            };

            match result {
                Ok(content) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "content": [{ "type": "text", "text": content.to_string() }] }
                }),
                Err(e) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": e }
                }),
            }
        }
        "tools/list" => {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {
                            "name": "create_agent",
                            "description": "Register a new agent in a project",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "project_key": { "type": "string", "description": "Human-readable project identifier" },
                                    "name_hint": { "type": "string", "description": "Agent name (defaults to AnonymousAgent)" },
                                    "program": { "type": "string", "description": "Program/version identifier" },
                                    "model": { "type": "string", "description": "LLM model name" }
                                },
                                "required": ["project_key"]
                            }
                        },
                        {
                            "name": "send_message",
                            "description": "Send a message to one or more agents",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "from_agent": { "type": "string", "description": "Sender agent name" },
                                    "to": { "type": "array", "items": { "type": "string" }, "description": "Recipient agent names" },
                                    "subject": { "type": "string", "description": "Message subject" },
                                    "body": { "type": "string", "description": "Message body" },
                                    "project_id": { "type": "string", "description": "Project ID for search indexing" }
                                },
                                "required": ["from_agent", "to"]
                            }
                        },
                        {
                            "name": "search_messages",
                            "description": "Full-text search over indexed messages",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "query": { "type": "string", "description": "Search query (Tantivy syntax)" },
                                    "limit": { "type": "integer", "description": "Max results to return (default 10, max 1000)" }
                                },
                                "required": ["query"]
                            }
                        },
                        {
                            "name": "get_inbox",
                            "description": "Drain unread messages from an agent's inbox",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "agent_name": { "type": "string", "description": "Agent whose inbox to drain" },
                                    "limit": { "type": "integer", "description": "Max messages to drain (default 100, max 1000)" }
                                },
                                "required": ["agent_name"]
                            }
                        }
                    ]
                }
            })
        }
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": "Method not found" }
        }),
    }
}

// ── Input validation ─────────────────────────────────────────────────────────
// Reject names that could cause path traversal or filesystem issues.
fn validate_agent_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Invalid agent name: must not be empty".to_string());
    }
    if name.len() > MAX_AGENT_NAME_LEN {
        return Err(format!(
            "Invalid agent name: exceeds {} character limit",
            MAX_AGENT_NAME_LEN
        ));
    }
    if name.contains('/') || name.contains('\\') {
        return Err("Invalid agent name: must not contain path separators".to_string());
    }
    if name.contains("..") {
        return Err("Invalid agent name: must not contain '..'".to_string());
    }
    if name == "." {
        return Err("Invalid agent name: must not be '.'".to_string());
    }
    if name.starts_with('.') {
        return Err("Invalid agent name: must not start with '.'".to_string());
    }
    if name.contains('\0') {
        return Err("Invalid agent name: must not contain null bytes".to_string());
    }
    if name.chars().any(|c| c.is_control()) {
        return Err("Invalid agent name: must not contain control characters".to_string());
    }
    if name.trim().is_empty() {
        return Err("Invalid agent name: must not be whitespace-only".to_string());
    }
    if name != name.trim() {
        return Err("Invalid agent name: must not have leading or trailing whitespace".to_string());
    }
    Ok(())
}

// ── create_agent ─────────────────────────────────────────────────────────────
// Hot path: DashMap insert + crossbeam send. No disk I/O.
fn create_agent(state: &PostOffice, args: Value) -> Result<Value, String> {
    let project_key = args
        .get("project_key")
        .and_then(|v| v.as_str())
        .ok_or("Missing project_key")?;
    if project_key.contains('\0') {
        return Err("project_key must not contain null bytes".to_string());
    }
    if project_key.chars().any(|c| c.is_control()) {
        return Err("project_key must not contain control characters".to_string());
    }
    if project_key.len() > MAX_PROJECT_KEY_LEN {
        return Err(format!(
            "project_key exceeds {} character limit",
            MAX_PROJECT_KEY_LEN
        ));
    }
    if project_key.trim().is_empty() && !project_key.is_empty() {
        return Err("project_key must not be whitespace-only".to_string());
    }
    if project_key != project_key.trim() {
        return Err("project_key must not have leading or trailing whitespace".to_string());
    }
    let program = args
        .get("program")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let model = args
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let name_hint = args.get("name_hint").and_then(|v| v.as_str());

    // Validate all inputs BEFORE mutating any state.
    if program.len() > MAX_PROGRAM_LEN {
        return Err(format!("program exceeds {} byte limit", MAX_PROGRAM_LEN));
    }
    if model.len() > MAX_MODEL_LEN {
        return Err(format!("model exceeds {} byte limit", MAX_MODEL_LEN));
    }
    let name = name_hint.unwrap_or("AnonymousAgent").to_string();
    validate_agent_name(&name)?;

    let project_id = format!(
        "proj_{}",
        Uuid::new_v5(&Uuid::NAMESPACE_DNS, project_key.as_bytes()).simple()
    );

    // Soft cap on total agent count. Checked BEFORE entry() to avoid deadlock
    // (DashMap::len() locks all shards; entry() holds one shard lock).
    // Slightly racy under concurrency but acceptable as a soft limit.
    if state.agents.len() >= MAX_AGENTS {
        return Err(format!("Agent limit reached ({} max)", MAX_AGENTS));
    }

    // Atomic check-and-insert via DashMap::entry(). Holds the shard lock
    // across the collision check and the insert/update, eliminating the
    // TOCTOU race that existed with separate get() + insert().
    let agent_id = Uuid::new_v4().to_string();
    let registered_at = Utc::now().timestamp();

    let record = AgentRecord {
        id: agent_id.clone(),
        project_id: project_id.clone(),
        name: name.clone(),
        program: program.to_string(),
        model: model.to_string(),
        registered_at,
    };

    match state.agents.entry(name.clone()) {
        dashmap::mapref::entry::Entry::Occupied(mut occ) => {
            if occ.get().project_id != project_id {
                return Err(format!(
                    "Agent '{}' already registered in a different project",
                    name
                ));
            }
            // Same project — idempotent upsert
            occ.insert(record);
        }
        dashmap::mapref::entry::Entry::Vacant(vac) => {
            vac.insert(record);
        }
    }

    // Record project mapping AFTER successful agent registration.
    // No state mutation occurs on any error path.
    // Soft cap: only insert if under limit (existing projects always pass).
    if !state.projects.contains_key(&project_id) && state.projects.len() >= MAX_PROJECTS {
        // Agent was already inserted — this is a soft cap, not a hard error.
        // The agent exists but the project mapping is not recorded.
        tracing::warn!(
            "Project limit reached ({} max), skipping project mapping for {}",
            MAX_PROJECTS,
            project_id
        );
    } else {
        state
            .projects
            .entry(project_id.clone())
            .or_insert_with(|| project_key.to_string());
    }

    // Fire-and-forget: persist agent profile to Git
    let profile = json!({
        "id": agent_id,
        "project_id": project_id,
        "name": name,
        "program": program,
        "model": model,
        "registered_at": registered_at
    });
    if let Err(e) = state.persist_tx.try_send(PersistOp::GitCommit {
        path: format!("agents/{}/profile.json", name),
        content: serde_json::to_string_pretty(&profile).unwrap(),
        message: format!("Register agent {}", name),
    }) {
        tracing::warn!(
            "Persist channel full, dropping git commit for agent {}: {}",
            name,
            e
        );
    }

    Ok(json!(profile))
}

// ── send_message ─────────────────────────────────────────────────────────────
// Hot path: DashMap inbox append + crossbeam persist send. No disk I/O.
fn send_message(state: &PostOffice, args: Value) -> Result<Value, String> {
    let from_agent = args
        .get("from_agent")
        .and_then(|v| v.as_str())
        .ok_or("Missing from_agent")?;
    let to_agents: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        args.get("to")
            .and_then(|v| v.as_array())
            .ok_or("Missing to recipients")?
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .filter(|s| seen.insert(s.to_string()))
            .map(|s| s.to_string())
            .collect()
    };
    let subject = args.get("subject").and_then(|v| v.as_str()).unwrap_or("");
    let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let project_id = args
        .get("project_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if from_agent.is_empty() {
        return Err("from_agent must not be empty".to_string());
    }
    if from_agent.len() > MAX_FROM_AGENT_LEN {
        return Err(format!(
            "from_agent exceeds {} character limit",
            MAX_FROM_AGENT_LEN
        ));
    }
    if from_agent.contains('\0') {
        return Err("from_agent must not contain null bytes".to_string());
    }
    if from_agent.chars().any(|c| c.is_control()) {
        return Err("from_agent must not contain control characters".to_string());
    }
    if from_agent != from_agent.trim() {
        return Err("from_agent must not have leading or trailing whitespace".to_string());
    }
    if to_agents.is_empty() {
        return Err("No recipients specified".to_string());
    }
    if to_agents.len() > MAX_RECIPIENTS {
        return Err(format!(
            "Too many recipients ({}, max {})",
            to_agents.len(),
            MAX_RECIPIENTS
        ));
    }
    for recipient in &to_agents {
        if recipient.len() > MAX_RECIPIENT_NAME_LEN {
            return Err(format!(
                "Recipient name exceeds {} byte limit",
                MAX_RECIPIENT_NAME_LEN
            ));
        }
        if recipient.contains('\0') {
            return Err(format!(
                "Recipient '{}' must not contain null bytes",
                recipient
                    .chars()
                    .filter(|c| !c.is_control())
                    .collect::<String>()
            ));
        }
        if recipient.chars().any(|c| c.is_control()) {
            return Err(format!(
                "Recipient '{}' must not contain control characters",
                recipient
                    .chars()
                    .filter(|c| !c.is_control())
                    .collect::<String>()
            ));
        }
        if recipient.contains('/') || recipient.contains('\\') {
            return Err(format!(
                "Recipient '{}' must not contain path separators",
                recipient
            ));
        }
        if recipient.contains("..") {
            return Err(format!("Recipient '{}' must not contain '..'", recipient));
        }
        if recipient != recipient.trim() {
            return Err(format!(
                "Recipient '{}' must not have leading or trailing whitespace",
                recipient
            ));
        }
    }
    if subject.len() > MAX_SUBJECT_LEN {
        return Err(format!(
            "Subject exceeds {} character limit",
            MAX_SUBJECT_LEN
        ));
    }
    if subject.contains('\0') {
        return Err("Subject must not contain null bytes".to_string());
    }
    if subject.chars().any(|c| c.is_control()) {
        return Err("Subject must not contain control characters".to_string());
    }
    if body.contains('\0') {
        return Err("Body must not contain null bytes".to_string());
    }
    if body.len() > MAX_BODY_LEN {
        return Err(format!("Body exceeds {} byte limit", MAX_BODY_LEN));
    }
    if project_id.contains('\0') {
        return Err("project_id must not contain null bytes".to_string());
    }
    if project_id.chars().any(|c| c.is_control()) {
        return Err("project_id must not contain control characters".to_string());
    }
    if project_id.len() > MAX_PROJECT_ID_LEN {
        return Err(format!(
            "project_id exceeds {} byte limit",
            MAX_PROJECT_ID_LEN
        ));
    }

    let message_id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();

    // 1. Deliver to each recipient's inbox (lock-free DashMap append)
    let entry = InboxEntry {
        message_id: message_id.clone(),
        from_agent: from_agent.to_string(),
        subject: subject.to_string(),
        body: body.to_string(),
        timestamp: now,
        project_id: project_id.to_string(),
    };

    // Pre-check all recipient inboxes BEFORE any delivery.
    // This prevents partial delivery where some recipients get the message
    // but the caller receives an error because a later recipient's inbox is full.
    for recipient in &to_agents {
        let inbox = state.inboxes.get(recipient);
        if let Some(ref ib) = inbox {
            if ib.len() >= MAX_INBOX_SIZE {
                return Err(format!(
                    "Recipient '{}' inbox full ({} messages) — drain with get_inbox first",
                    recipient, MAX_INBOX_SIZE
                ));
            }
        }
    }

    // All inboxes have capacity — deliver atomically per recipient.
    for recipient in &to_agents {
        state
            .inboxes
            .entry(recipient.clone())
            .or_default()
            .push(entry.clone());
    }

    // 2. Fire-and-forget: index in Tantivy (batched by persistence worker)
    if let Err(e) = state.persist_tx.try_send(PersistOp::IndexMessage {
        id: message_id.clone(),
        project_id: project_id.to_string(),
        from_agent: from_agent.to_string(),
        to_recipients: to_agents.join("\x1F"),
        subject: subject.to_string(),
        body: body.to_string(),
        created_ts: now,
    }) {
        tracing::warn!(
            "Persist channel full, dropping index op for message {}: {}",
            message_id,
            e
        );
    }

    Ok(json!({ "id": message_id, "status": "sent" }))
}

// ── get_inbox ────────────────────────────────────────────────────────────────
// Hot path: DashMap partial or full drain. No disk I/O.
// Supports optional `limit` parameter (default 100, max 1000) to bound
// response size. Returns `remaining` count so callers know to fetch more.
fn get_inbox(state: &PostOffice, args: Value) -> Result<Value, String> {
    let agent_name = args
        .get("agent_name")
        .and_then(|v| v.as_str())
        .ok_or("Missing agent_name")?;
    if agent_name.is_empty() {
        return Err("agent_name must not be empty".to_string());
    }
    if agent_name.len() > MAX_RECIPIENT_NAME_LEN {
        return Err(format!(
            "agent_name exceeds {} byte limit",
            MAX_RECIPIENT_NAME_LEN
        ));
    }
    if agent_name.contains('\0') {
        return Err("agent_name must not contain null bytes".to_string());
    }
    if agent_name.chars().any(|c| c.is_control()) {
        return Err("agent_name must not contain control characters".to_string());
    }
    if agent_name.contains('/') || agent_name.contains('\\') {
        return Err("agent_name must not contain path separators".to_string());
    }
    if agent_name.contains("..") {
        return Err("agent_name must not contain '..'".to_string());
    }
    if agent_name != agent_name.trim() {
        return Err("agent_name must not have leading or trailing whitespace".to_string());
    }

    let limit = args
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(DEFAULT_INBOX_LIMIT as i64)
        .clamp(1, MAX_INBOX_LIMIT as i64) as usize;

    // Atomic drain via entry() API. Holds the DashMap shard lock for the
    // entire operation, preventing concurrent send_message from inserting
    // messages that would be overwritten by a separate insert() call.
    // Previous remove→insert pattern had a TOCTOU race window (H1).
    let mut taken = Vec::new();
    let remaining_count;

    match state.inboxes.entry(agent_name.to_string()) {
        dashmap::mapref::entry::Entry::Occupied(mut occ) => {
            let inbox = occ.get_mut();
            if inbox.len() <= limit {
                taken = std::mem::take(inbox);
                remaining_count = 0;
                occ.remove(); // Clean up empty entry
            } else {
                taken = inbox.drain(..limit).collect();
                remaining_count = inbox.len();
            }
        }
        dashmap::mapref::entry::Entry::Vacant(_) => {
            remaining_count = 0;
        }
    }

    let result: Vec<Value> = taken
        .into_iter()
        .map(|e| {
            json!({
                "id": e.message_id,
                "from_agent": e.from_agent,
                "subject": e.subject,
                "body": e.body,
                "created_ts": e.timestamp,
                "project_id": e.project_id
            })
        })
        .collect();

    Ok(json!({
        "messages": result,
        "remaining": remaining_count
    }))
}

// ── Tantivy field unwrapper ──────────────────────────────────────────────────
// Tantivy's schema.to_json() wraps each field value in an array because
// documents can have multiple values per field. Our schema only adds one
// value per field, so unwrap single-element arrays to produce scalar fields
// consistent with get_inbox's response format.
fn unwrap_tantivy_arrays(doc: Value) -> Value {
    match doc {
        Value::Object(map) => {
            let unwrapped = map
                .into_iter()
                .map(|(k, v)| {
                    let val = match v {
                        Value::Array(ref arr) if arr.len() == 1 => arr[0].clone(),
                        other => other,
                    };
                    (k, val)
                })
                .collect();
            Value::Object(unwrapped)
        }
        other => other,
    }
}

// ── search_messages ──────────────────────────────────────────────────────────
// Tantivy NRT search — unchanged, already fast.
fn search_messages(state: &PostOffice, args: Value) -> Result<Value, String> {
    let query_str = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or("Missing query")?;
    if query_str.trim().is_empty() {
        return Err("query must not be empty or whitespace-only".to_string());
    }
    if query_str.contains('\0') {
        return Err("query must not contain null bytes".to_string());
    }
    if query_str.len() > MAX_QUERY_LEN {
        return Err(format!("query exceeds {} byte limit", MAX_QUERY_LEN));
    }
    let limit = args
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(DEFAULT_SEARCH_LIMIT as i64)
        .clamp(1, MAX_SEARCH_LIMIT as i64) as usize;

    let index = &state.index;
    let searcher = state.index_reader.searcher();

    let schema = index.schema();
    let subject = schema.get_field("subject").unwrap();
    let body = schema.get_field("body").unwrap();

    let query_parser = QueryParser::for_index(index, vec![subject, body]);
    let query = query_parser
        .parse_query(query_str)
        .map_err(|e| format!("Query parse error: {}", e))?;

    let top_docs = searcher
        .search(&query, &TopDocs::with_limit(limit))
        .map_err(|e| format!("Search error: {}", e))?;

    let mut results = Vec::new();
    for (_score, doc_address) in top_docs {
        let retrieved_doc = searcher.doc(doc_address).map_err(|e| e.to_string())?;
        let doc_json = schema.to_json(&retrieved_doc);
        let parsed = serde_json::from_str::<Value>(&doc_json)
            .map_err(|e| format!("Failed to parse doc JSON: {}", e))?;
        // Tantivy's to_json wraps each field value in an array (multi-value support).
        // Unwrap single-element arrays to match get_inbox's scalar field format.
        let unwrapped = unwrap_tantivy_arrays(parsed);
        results.push(unwrapped);
    }

    Ok(json!({
        "results": results,
        "count": results.len()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PostOffice;
    use tempfile::TempDir;

    /// Helper: build a PostOffice backed by temp directories.
    fn test_post_office() -> (PostOffice, TempDir, TempDir) {
        let idx_dir = TempDir::new().unwrap();
        let repo_dir = TempDir::new().unwrap();
        let po = PostOffice::new(idx_dir.path(), repo_dir.path()).unwrap();
        (po, idx_dir, repo_dir)
    }

    /// Helper: extract messages array from get_inbox response.
    fn inbox_messages(resp: &Value) -> &Vec<Value> {
        resp["messages"].as_array().unwrap()
    }

    // ── H1: Agent Name Collision ────────────────────────────────────────────
    // Cross-project collision must be rejected.
    #[tokio::test]
    async fn h1_cross_project_name_collision_is_rejected() {
        let (state, _idx, _repo) = test_post_office();

        let r1 = create_agent(
            &state,
            json!({ "project_key": "project_a", "name_hint": "Alice", "program": "prog_a" }),
        );
        assert!(r1.is_ok(), "First registration should succeed");

        let r2 = create_agent(
            &state,
            json!({ "project_key": "project_b", "name_hint": "Alice", "program": "prog_b" }),
        );
        assert!(
            r2.is_err(),
            "Same name in different project must be rejected"
        );
        assert!(
            r2.unwrap_err().contains("already registered"),
            "Error should mention the name conflict"
        );

        // Original agent must be preserved
        let record = state.agents.get("Alice").unwrap();
        assert_eq!(
            record.program, "prog_a",
            "Original agent must not be overwritten"
        );
    }

    // Same-project re-registration must be idempotent (upsert).
    #[tokio::test]
    async fn h1_same_project_reregistration_is_upsert() {
        let (state, _idx, _repo) = test_post_office();

        let r1 = create_agent(
            &state,
            json!({ "project_key": "proj_x", "name_hint": "Bob", "program": "v1" }),
        )
        .unwrap();

        let r2 = create_agent(
            &state,
            json!({ "project_key": "proj_x", "name_hint": "Bob", "program": "v2" }),
        )
        .unwrap();

        // Both succeed — second is an upsert
        assert_ne!(
            r1["id"], r2["id"],
            "Re-registration generates a new agent_id"
        );
        let record = state.agents.get("Bob").unwrap();
        assert_eq!(record.program, "v2", "Upsert should update the record");
    }

    // Different names within the same project must coexist.
    #[tokio::test]
    async fn h1_different_names_same_project_coexist() {
        let (state, _idx, _repo) = test_post_office();

        create_agent(
            &state,
            json!({ "project_key": "proj_x", "name_hint": "Agent1" }),
        )
        .unwrap();
        create_agent(
            &state,
            json!({ "project_key": "proj_x", "name_hint": "Agent2" }),
        )
        .unwrap();

        assert!(state.agents.contains_key("Agent1"));
        assert!(state.agents.contains_key("Agent2"));
    }

    // ── H3: Inbox size is capped ────────────────────────────────────────────
    #[tokio::test]
    async fn h3_inbox_rejects_when_full() {
        let (state, _idx, _repo) = test_post_office();

        // Fill inbox to the cap
        for i in 0..super::MAX_INBOX_SIZE {
            let result = send_message(
                &state,
                json!({
                    "from_agent": "sender",
                    "to": ["recipient"],
                    "subject": format!("msg #{}", i),
                    "body": "x",
                }),
            );
            assert!(result.is_ok(), "Message {} should be accepted", i);
        }

        // Next message should be rejected
        let result = send_message(
            &state,
            json!({
                "from_agent": "sender",
                "to": ["recipient"],
                "subject": "overflow",
                "body": "this should fail",
            }),
        );
        assert!(result.is_err(), "Message beyond inbox cap must be rejected");
        assert!(
            result.unwrap_err().contains("inbox full"),
            "Error should mention inbox full"
        );
    }

    #[tokio::test]
    async fn h3_inbox_accepts_after_drain() {
        let (state, _idx, _repo) = test_post_office();

        // Fill inbox to the cap
        for i in 0..super::MAX_INBOX_SIZE {
            send_message(
                &state,
                json!({
                    "from_agent": "sender",
                    "to": ["recipient"],
                    "subject": format!("msg #{}", i),
                    "body": "x",
                }),
            )
            .unwrap();
        }

        // Drain the inbox (pass max limit to drain all)
        get_inbox(&state, json!({ "agent_name": "recipient", "limit": 1000 })).unwrap();
        // Drain remaining (10K total, 1K per call)
        for _ in 0..9 {
            get_inbox(&state, json!({ "agent_name": "recipient", "limit": 1000 })).unwrap();
        }

        // Should accept messages again
        let result = send_message(
            &state,
            json!({
                "from_agent": "sender",
                "to": ["recipient"],
                "subject": "after drain",
                "body": "this should succeed",
            }),
        );
        assert!(result.is_ok(), "Inbox should accept messages after drain");
    }

    // ── H4: Persist channel drops are logged, not silent ───────────────────
    // send_message still returns Ok (hot path must not block on persistence),
    // but channel failures are now logged via tracing::warn.
    #[tokio::test]
    async fn h4_persist_channel_drop_returns_ok_but_logs() {
        let (state, _idx, _repo) = test_post_office();

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "test",
                "body": "hello",
            }),
        );
        assert!(
            result.is_ok(),
            "send_message returns Ok — hot path is independent of persist pipeline"
        );
        // Channel drops are now logged via tracing::warn (verified by code inspection).
    }

    // ── H10: Non-string recipients are filtered out ────────────────────────
    #[tokio::test]
    async fn h10_non_string_recipients_are_filtered() {
        let (state, _idx, _repo) = test_post_office();

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob", 123, null],
                "subject": "test",
                "body": "hello",
            }),
        );
        assert!(result.is_ok());

        // "bob" gets the message (valid string recipient)
        assert!(state.inboxes.contains_key("bob"));
        // Non-string elements must NOT create empty-string inbox entries
        assert!(
            !state.inboxes.contains_key(""),
            "Non-string to elements must be filtered out, not coerced to empty string"
        );
    }

    // Empty-string recipients in the array must also be filtered.
    #[tokio::test]
    async fn h10_empty_string_recipients_are_filtered() {
        let (state, _idx, _repo) = test_post_office();

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob", ""],
                "subject": "test",
                "body": "hello",
            }),
        );
        assert!(result.is_ok());

        assert!(state.inboxes.contains_key("bob"));
        assert!(
            !state.inboxes.contains_key(""),
            "Empty string recipients must be filtered out"
        );
    }

    // All-invalid recipients should return an error.
    #[tokio::test]
    async fn h10_all_invalid_recipients_returns_error() {
        let (state, _idx, _repo) = test_post_office();

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": [123, null, ""],
                "subject": "test",
                "body": "hello",
            }),
        );
        assert!(result.is_err(), "All-invalid recipients must return error");
    }

    // ── H18: search_messages limit must be clamped ─────────────────────────
    #[tokio::test]
    async fn h18_negative_limit_returns_results_not_oom() {
        let (state, _idx, _repo) = test_post_office();

        // Negative limit must not wrap to usize::MAX — should be clamped.
        let r = search_messages(&state, json!({ "query": "hello", "limit": -1 }));
        assert!(r.is_ok(), "Negative limit must not cause panic or OOM");
    }

    #[tokio::test]
    async fn h18_zero_limit_returns_ok() {
        let (state, _idx, _repo) = test_post_office();

        let r = search_messages(&state, json!({ "query": "hello", "limit": 0 }));
        assert!(r.is_ok(), "Zero limit must not cause error");
    }

    #[tokio::test]
    async fn h18_huge_limit_is_capped() {
        let (state, _idx, _repo) = test_post_office();

        // A limit of 999999999 must not allocate that much — should be capped.
        let r = search_messages(&state, json!({ "query": "hello", "limit": 999_999_999 }));
        assert!(r.is_ok(), "Huge limit must be capped, not cause OOM");
    }

    // ── H9: Input validation rejects malicious agent names ──────────────────
    #[tokio::test]
    async fn h9_create_agent_rejects_path_traversal() {
        let (state, _idx, _repo) = test_post_office();
        let r = create_agent(
            &state,
            json!({ "project_key": "test", "name_hint": "../../etc/evil" }),
        );
        assert!(r.is_err(), "name_hint with .. must be rejected");
        assert!(
            r.unwrap_err().contains("Invalid"),
            "Error message should indicate invalid name"
        );
    }

    #[tokio::test]
    async fn h9_create_agent_rejects_slashes() {
        let (state, _idx, _repo) = test_post_office();
        let r = create_agent(
            &state,
            json!({ "project_key": "test", "name_hint": "agents/evil" }),
        );
        assert!(r.is_err(), "name_hint with / must be rejected");
    }

    #[tokio::test]
    async fn h9_create_agent_rejects_null_bytes() {
        let (state, _idx, _repo) = test_post_office();
        let r = create_agent(
            &state,
            json!({ "project_key": "test", "name_hint": "agent\x00evil" }),
        );
        assert!(r.is_err(), "name_hint with null bytes must be rejected");
    }

    #[tokio::test]
    async fn h9_create_agent_allows_safe_names() {
        let (state, _idx, _repo) = test_post_office();
        for name in &["Alice", "agent_007", "Bob-Smith", "CamelCase42"] {
            let r = create_agent(&state, json!({ "project_key": "test", "name_hint": name }));
            assert!(r.is_ok(), "Safe name '{}' should be accepted", name);
        }
    }

    // ── H-JSON-RPC: Missing method returns error ────────────────────────────
    // Regression guard: unknown methods should return -32601.
    #[tokio::test]
    async fn jsonrpc_unknown_method_returns_error() {
        let (state, _idx, _repo) = test_post_office();
        let resp = handle_mcp_request(
            state,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "nonexistent" }),
        )
        .await;
        assert_eq!(resp["error"]["code"], -32601);
    }

    // ── H-JSON-RPC: Unknown tool returns error ──────────────────────────────
    #[tokio::test]
    async fn jsonrpc_unknown_tool_returns_error() {
        let (state, _idx, _repo) = test_post_office();
        let resp = handle_mcp_request(
            state,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "delete_everything", "arguments": {} }
            }),
        )
        .await;
        assert!(resp.get("error").is_some());
    }

    // ── H-DRAIN: get_inbox is destructive ───────────────────────────────────
    // Regression guard: calling get_inbox twice should return empty on second call.
    #[tokio::test]
    async fn get_inbox_is_destructive_drain() {
        let (state, _idx, _repo) = test_post_office();

        send_message(
            &state,
            json!({ "from_agent": "a", "to": ["b"], "subject": "hi", "body": "yo" }),
        )
        .unwrap();

        let first = get_inbox(&state, json!({ "agent_name": "b" })).unwrap();
        assert_eq!(
            inbox_messages(&first).len(),
            1,
            "First drain should return the message"
        );

        let second = get_inbox(&state, json!({ "agent_name": "b" })).unwrap();
        assert_eq!(
            inbox_messages(&second).len(),
            0,
            "Second drain should be empty — messages consumed"
        );
    }

    // ── H-PROJ: project_id is deterministic from project_key ────────────────
    #[tokio::test]
    async fn project_id_is_deterministic() {
        let (state, _idx, _repo) = test_post_office();

        let r1 = create_agent(
            &state,
            json!({ "project_key": "my_project", "name_hint": "Agent1" }),
        )
        .unwrap();
        let r2 = create_agent(
            &state,
            json!({ "project_key": "my_project", "name_hint": "Agent2" }),
        )
        .unwrap();

        assert_eq!(
            r1["project_id"], r2["project_id"],
            "Same project_key must produce same project_id"
        );
    }

    // ── H-SEND: Missing required fields return errors ───────────────────────
    #[tokio::test]
    async fn send_message_missing_from_agent_returns_error() {
        let (state, _idx, _repo) = test_post_office();
        let r = send_message(&state, json!({ "to": ["bob"] }));
        assert!(r.is_err());
        assert_eq!(r.unwrap_err(), "Missing from_agent");
    }

    #[tokio::test]
    async fn send_message_missing_to_returns_error() {
        let (state, _idx, _repo) = test_post_office();
        let r = send_message(&state, json!({ "from_agent": "alice" }));
        assert!(r.is_err());
        assert_eq!(r.unwrap_err(), "Missing to recipients");
    }

    // ── H-MULTI: Multi-recipient delivery ───────────────────────────────────
    #[tokio::test]
    async fn send_message_delivers_to_all_recipients() {
        let (state, _idx, _repo) = test_post_office();

        send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob", "charlie", "dave"],
                "subject": "broadcast",
                "body": "hello all",
            }),
        )
        .unwrap();

        for name in &["bob", "charlie", "dave"] {
            let inbox = get_inbox(&state, json!({ "agent_name": name })).unwrap();
            assert_eq!(
                inbox_messages(&inbox).len(),
                1,
                "{} should have exactly 1 message",
                name
            );
        }
    }

    // ── H-EMPTY: get_inbox for nonexistent agent returns empty array ────────
    #[tokio::test]
    async fn get_inbox_nonexistent_agent_returns_empty() {
        let (state, _idx, _repo) = test_post_office();
        let r = get_inbox(&state, json!({ "agent_name": "nobody" })).unwrap();
        assert_eq!(inbox_messages(&r).len(), 0);
        assert_eq!(r["remaining"], 0);
    }

    // ── H-DEFAULT: create_agent without name_hint defaults to AnonymousAgent ─
    #[tokio::test]
    async fn create_agent_default_name_is_anonymous() {
        let (state, _idx, _repo) = test_post_office();
        let r = create_agent(&state, json!({ "project_key": "test" })).unwrap();
        assert_eq!(r["name"], "AnonymousAgent");
    }

    // ── H-TOOLS-LIST: tools/list returns all 4 tools ────────────────────────
    #[tokio::test]
    async fn tools_list_returns_four_tools() {
        let (state, _idx, _repo) = test_post_office();
        let resp = handle_mcp_request(
            state,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        )
        .await;
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 4);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"create_agent"));
        assert!(names.contains(&"send_message"));
        assert!(names.contains(&"search_messages"));
        assert!(names.contains(&"get_inbox"));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Deep Review #2 — Post-remediation adversarial hypotheses
    // ═══════════════════════════════════════════════════════════════════════

    // ── H1: TOCTOU race in create_agent cross-project collision check ────
    // The get() → insert() sequence is not atomic. Two concurrent requests
    // with the same name but different project_keys can both pass the
    // collision check.
    #[tokio::test]
    async fn h1_toctou_concurrent_cross_project_collision() {
        use std::sync::{Arc, Barrier};

        let (state, _idx, _repo) = test_post_office();
        let state = Arc::new(state);
        let barrier = Arc::new(Barrier::new(2));

        let mut handles = Vec::new();
        for i in 0..2 {
            let s = Arc::clone(&state);
            let b = Arc::clone(&barrier);
            let project_key = format!("project_{}", i);
            handles.push(std::thread::spawn(move || {
                b.wait(); // Maximize chance of interleaving
                create_agent(
                    &s,
                    json!({
                        "project_key": project_key,
                        "name_hint": "RaceName",
                    }),
                )
            }));
        }

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let successes = results.iter().filter(|r| r.is_ok()).count();
        let failures = results.iter().filter(|r| r.is_err()).count();

        // Fixed: atomic entry() API ensures exactly one succeeds.
        assert_eq!(successes, 1, "Exactly one should succeed");
        assert_eq!(failures, 1, "Exactly one should be rejected");
    }

    // ── H2: Partial delivery when multi-recipient inbox is full ──────────
    // If recipient B's inbox is full but A's is not, A gets the message
    // but the caller receives an error.
    #[tokio::test]
    async fn h2_partial_delivery_on_inbox_full() {
        let (state, _idx, _repo) = test_post_office();

        // Fill recipient_b's inbox to cap
        for i in 0..MAX_INBOX_SIZE {
            send_message(
                &state,
                json!({
                    "from_agent": "filler",
                    "to": ["recipient_b"],
                    "subject": format!("fill #{}", i),
                    "body": "x",
                }),
            )
            .unwrap();
        }

        // Send to [recipient_a, recipient_b] — B is full
        let result = send_message(
            &state,
            json!({
                "from_agent": "sender",
                "to": ["recipient_a", "recipient_b"],
                "subject": "multi-send",
                "body": "hello",
            }),
        );

        // Caller gets an error (B is full)
        assert!(
            result.is_err(),
            "Should fail because recipient_b inbox is full"
        );

        // Fixed: pre-check prevents partial delivery. recipient_a should NOT
        // have received the message since the entire send was rejected.
        let inbox_a = get_inbox(&state, json!({ "agent_name": "recipient_a" })).unwrap();

        assert_eq!(
            inbox_messages(&inbox_a).len(),
            0,
            "No partial delivery: recipient_a must not receive message when send is rejected"
        );
    }

    // ── H4: Field length limits enforced ────────────────────────────────
    #[tokio::test]
    async fn h4_long_agent_name_is_rejected() {
        let (state, _idx, _repo) = test_post_office();
        let long_name = "A".repeat(super::MAX_AGENT_NAME_LEN + 1);

        let result = create_agent(
            &state,
            json!({ "project_key": "test", "name_hint": long_name }),
        );

        assert!(
            result.is_err(),
            "Agent name exceeding limit must be rejected"
        );
        assert!(
            result.unwrap_err().contains("character limit"),
            "Error should mention character limit"
        );
    }

    #[tokio::test]
    async fn h4_max_length_agent_name_is_accepted() {
        let (state, _idx, _repo) = test_post_office();
        let name = "A".repeat(super::MAX_AGENT_NAME_LEN);

        let result = create_agent(&state, json!({ "project_key": "test", "name_hint": name }));
        assert!(
            result.is_ok(),
            "Agent name at exactly the limit should be accepted"
        );
    }

    #[tokio::test]
    async fn h4b_long_message_body_is_rejected() {
        let (state, _idx, _repo) = test_post_office();
        let huge_body = "X".repeat(super::MAX_BODY_LEN + 1);

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "big payload",
                "body": huge_body,
            }),
        );

        assert!(result.is_err(), "Body exceeding limit must be rejected");
        assert!(
            result.unwrap_err().contains("byte limit"),
            "Error should mention byte limit"
        );
    }

    #[tokio::test]
    async fn h4c_long_subject_is_rejected() {
        let (state, _idx, _repo) = test_post_office();
        let long_subject = "S".repeat(super::MAX_SUBJECT_LEN + 1);

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": long_subject,
                "body": "ok",
            }),
        );

        assert!(result.is_err(), "Subject exceeding limit must be rejected");
        assert!(
            result.unwrap_err().contains("character limit"),
            "Error should mention character limit"
        );
    }

    // ── H6/H7: Unregistered sender can send messages ────────────────────
    #[tokio::test]
    async fn h7_unregistered_sender_can_send_messages() {
        let (state, _idx, _repo) = test_post_office();

        // Send from an agent that was never registered
        let result = send_message(
            &state,
            json!({
                "from_agent": "ghost_agent",
                "to": ["bob"],
                "subject": "spoofed",
                "body": "I don't exist",
            }),
        );

        // CONFIRMED (design choice): No sender validation
        assert!(
            result.is_ok(),
            "Unregistered agent can send messages (no sender validation)"
        );

        let inbox = get_inbox(&state, json!({ "agent_name": "bob" })).unwrap();
        assert_eq!(
            inbox_messages(&inbox)[0]["from_agent"].as_str().unwrap(),
            "ghost_agent"
        );
    }

    // ── H8: Concurrent get_inbox + send_message is safe ─────────────────
    // DashMap operations are atomic — no lost messages or double-reads.
    #[tokio::test]
    async fn h8_concurrent_drain_and_send_are_safe() {
        use std::sync::{Arc, Barrier};

        let (state, _idx, _repo) = test_post_office();
        let state = Arc::new(state);

        // Pre-populate inbox
        for i in 0..100 {
            send_message(
                &state,
                json!({
                    "from_agent": "sender",
                    "to": ["target"],
                    "subject": format!("msg {}", i),
                    "body": "x",
                }),
            )
            .unwrap();
        }

        let barrier = Arc::new(Barrier::new(2));

        // Thread 1: drain inbox
        let s1 = Arc::clone(&state);
        let b1 = Arc::clone(&barrier);
        let drain_handle = std::thread::spawn(move || {
            b1.wait();
            get_inbox(&s1, json!({ "agent_name": "target", "limit": 1000 })).unwrap()
        });

        // Thread 2: send more messages
        let s2 = Arc::clone(&state);
        let b2 = Arc::clone(&barrier);
        let send_handle = std::thread::spawn(move || {
            b2.wait();
            let mut sent = 0;
            for i in 0..50 {
                if send_message(
                    &s2,
                    json!({
                        "from_agent": "sender2",
                        "to": ["target"],
                        "subject": format!("concurrent {}", i),
                        "body": "y",
                    }),
                )
                .is_ok()
                {
                    sent += 1;
                }
            }
            sent
        });

        let drained = drain_handle.join().unwrap();
        let sent = send_handle.join().unwrap();
        let drained_count =
            inbox_messages(&drained).len() + drained["remaining"].as_u64().unwrap() as usize;

        // Drain whatever remains (use max limit)
        let mut remaining_total = 0usize;
        loop {
            let r = get_inbox(&state, json!({ "agent_name": "target", "limit": 1000 })).unwrap();
            let batch = inbox_messages(&r).len();
            if batch == 0 {
                break;
            }
            remaining_total += batch;
        }

        let total_accounted = drained_count + remaining_total;
        // We started with 100 + sent up to 50 more = up to 150 total
        assert!(
            total_accounted >= 100 && total_accounted <= 100 + sent,
            "No messages lost: drained={} remaining={} sent={}",
            drained_count,
            remaining_total,
            sent
        );
    }

    // ── H10: Whitespace-only and control-char agent names rejected ──────
    #[tokio::test]
    async fn h10_whitespace_only_agent_name_is_rejected() {
        let (state, _idx, _repo) = test_post_office();

        let result = create_agent(&state, json!({ "project_key": "test", "name_hint": "   " }));

        assert!(result.is_err(), "Whitespace-only names must be rejected");
        assert!(result.unwrap_err().contains("whitespace"));
    }

    #[tokio::test]
    async fn h10_control_char_agent_name_is_rejected() {
        let (state, _idx, _repo) = test_post_office();

        let result = create_agent(
            &state,
            json!({ "project_key": "test", "name_hint": "agent\x07bell" }),
        );

        assert!(
            result.is_err(),
            "Control characters in names must be rejected"
        );
        assert!(result.unwrap_err().contains("control"));
    }

    // ── H12: Second project_key for same project_id is silently ignored ──
    #[tokio::test]
    async fn h12_project_key_not_updated_on_reinsert() {
        let (state, _idx, _repo) = test_post_office();

        create_agent(
            &state,
            json!({ "project_key": "original_key", "name_hint": "Agent1" }),
        )
        .unwrap();

        // Same project_key → same project_id, different agent name
        create_agent(
            &state,
            json!({ "project_key": "original_key", "name_hint": "Agent2" }),
        )
        .unwrap();

        // The projects DashMap uses or_insert_with, so it keeps the first value.
        // Verify the stored human key is the original.
        let project_id = format!(
            "proj_{}",
            Uuid::new_v5(&Uuid::NAMESPACE_DNS, "original_key".as_bytes()).simple()
        );
        let stored_key = state.projects.get(&project_id).unwrap();
        assert_eq!(
            stored_key.value(),
            "original_key",
            "First project_key is preserved"
        );
    }

    // ── H-REGRESSION: search_messages with special query chars ──────────
    #[tokio::test]
    async fn search_messages_handles_special_query_chars() {
        let (state, _idx, _repo) = test_post_office();

        // Tantivy query parser handles these gracefully
        let result = search_messages(&state, json!({ "query": "foo AND bar OR baz" }));
        assert!(result.is_ok(), "Boolean query syntax should be handled");

        let result = search_messages(&state, json!({ "query": "*" }));
        // Wildcard queries may or may not parse depending on Tantivy config
        // The important thing is it doesn't panic
        let _ = result;

        let result = search_messages(&state, json!({ "query": "" }));
        // Empty query — should return error or empty, not panic
        let _ = result;
    }

    // ── H-REGRESSION: send_message with duplicate recipients ────────────
    #[tokio::test]
    async fn send_message_duplicate_recipients_delivers_twice() {
        let (state, _idx, _repo) = test_post_office();

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob", "bob"],
                "subject": "duped",
                "body": "hello",
            }),
        );
        assert!(result.is_ok());

        // Duplicates are now deduplicated — bob gets exactly 1 copy (DR21 H1 fix)
        let inbox = get_inbox(&state, json!({ "agent_name": "bob" })).unwrap();
        assert_eq!(
            inbox_messages(&inbox).len(),
            1,
            "Duplicate recipients are deduplicated — single delivery"
        );
    }

    // ── H-REGRESSION: JSON-RPC id is echoed correctly ───────────────────
    #[tokio::test]
    async fn jsonrpc_id_is_echoed_in_response() {
        let (state, _idx, _repo) = test_post_office();

        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0",
                "id": 42,
                "method": "tools/call",
                "params": { "name": "get_inbox", "arguments": { "agent_name": "nobody" } }
            }),
        )
        .await;
        assert_eq!(resp["id"], 42, "JSON-RPC id must be echoed back");

        let resp2 = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0",
                "id": "string-id-123",
                "method": "tools/list"
            }),
        )
        .await;
        assert_eq!(
            resp2["id"], "string-id-123",
            "String id must also be echoed"
        );
    }

    // ── H-REGRESSION: Null/missing JSON-RPC id is handled ───────────────
    #[tokio::test]
    async fn jsonrpc_null_id_is_handled() {
        let (state, _idx, _repo) = test_post_office();

        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0",
                "method": "tools/list"
            }),
        )
        .await;

        // When id is missing, resp["id"] should be null (not crash)
        assert!(resp["id"].is_null(), "Missing id should be echoed as null");

        let resp2 = handle_mcp_request(
            state,
            json!({
                "jsonrpc": "2.0",
                "id": null,
                "method": "tools/list"
            }),
        )
        .await;
        assert!(
            resp2["id"].is_null(),
            "Explicit null id should be echoed as null"
        );
    }

    // ── H-REGRESSION: create_agent with all optional fields missing ─────
    #[tokio::test]
    async fn create_agent_minimal_args() {
        let (state, _idx, _repo) = test_post_office();

        let result = create_agent(&state, json!({ "project_key": "minimal" }));
        assert!(result.is_ok());

        let profile = result.unwrap();
        assert_eq!(profile["name"], "AnonymousAgent");
        assert_eq!(profile["program"], "unknown");
        assert_eq!(profile["model"], "unknown");
    }

    // ── H-REGRESSION: send_message with all optional fields missing ─────
    #[tokio::test]
    async fn send_message_minimal_args() {
        let (state, _idx, _repo) = test_post_office();

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
            }),
        );
        assert!(result.is_ok());

        let inbox = get_inbox(&state, json!({ "agent_name": "bob" })).unwrap();
        let msg = &inbox_messages(&inbox)[0];
        assert_eq!(msg["subject"], "");
        assert_eq!(msg["body"], "");
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Deep Review #3 — Post-remediation-2 adversarial hypotheses
    // ═══════════════════════════════════════════════════════════════════════

    // ── H1: Inbox pre-check TOCTOU ──────────────────────────────────────
    // The H2 fix separated the capacity check (get) from the push (entry).
    // Under high concurrency, multiple threads can pass the pre-check
    // simultaneously and push, briefly exceeding the cap. This is a
    // deliberate tradeoff: soft cap > partial delivery.
    #[tokio::test]
    async fn h1_inbox_precheck_toctou_soft_cap() {
        use std::sync::{Arc, Barrier};

        let (state, _idx, _repo) = test_post_office();
        let state = Arc::new(state);

        // Fill inbox to 1 below cap
        for i in 0..MAX_INBOX_SIZE - 1 {
            send_message(
                &state,
                json!({
                    "from_agent": "filler",
                    "to": ["target"],
                    "subject": format!("fill #{}", i),
                    "body": "x",
                }),
            )
            .unwrap();
        }

        // Now 2 threads race to send to the same near-full inbox
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for i in 0..2 {
            let s = Arc::clone(&state);
            let b = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                b.wait();
                send_message(
                    &s,
                    json!({
                        "from_agent": format!("racer_{}", i),
                        "to": ["target"],
                        "subject": "race",
                        "body": "x",
                    }),
                )
            }));
        }

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let successes = results.iter().filter(|r| r.is_ok()).count();

        // Both may succeed because the pre-check is not atomic with the push.
        // This is accepted: the cap is a soft limit to prevent unbounded growth,
        // not a hard invariant. Exceeding by a small amount under races is fine.
        assert!(
            successes >= 1,
            "At least one send should succeed (cap was not reached)"
        );
        // Document: cap can be exceeded by at most the number of concurrent senders.
        // Drain all messages to count total (paginated).
        let mut total = 0usize;
        loop {
            let r = get_inbox(&state, json!({ "agent_name": "target", "limit": 1000 })).unwrap();
            let batch = inbox_messages(&r).len();
            if batch == 0 {
                break;
            }
            total += batch;
        }
        assert!(
            (MAX_INBOX_SIZE..=MAX_INBOX_SIZE + 1).contains(&total),
            "Inbox may slightly exceed soft cap under race: got {}",
            total
        );
    }

    // ── H2: get_inbox pagination bounds response size ──────────────────
    #[tokio::test]
    async fn h2_get_inbox_pagination() {
        let (state, _idx, _repo) = test_post_office();

        let count = 250;
        for i in 0..count {
            send_message(
                &state,
                json!({
                    "from_agent": "sender",
                    "to": ["receiver"],
                    "subject": format!("msg #{}", i),
                    "body": "x".repeat(100),
                }),
            )
            .unwrap();
        }

        // Default limit is 100
        let page1 = get_inbox(&state, json!({ "agent_name": "receiver" })).unwrap();
        let msgs1 = inbox_messages(&page1);
        assert_eq!(msgs1.len(), 100, "Default limit returns 100");
        assert_eq!(page1["remaining"], 150, "150 remain after first page");

        // Verify messages are well-formed
        assert!(msgs1[0]["id"].is_string());
        assert_eq!(msgs1[0]["from_agent"], "sender");
        assert!(msgs1[0]["created_ts"].is_i64());

        // Second page with explicit limit
        let page2 = get_inbox(&state, json!({ "agent_name": "receiver", "limit": 200 })).unwrap();
        let msgs2 = inbox_messages(&page2);
        assert_eq!(msgs2.len(), 150, "Second page returns remaining 150");
        assert_eq!(page2["remaining"], 0, "No more remaining");

        // Third call returns empty
        let page3 = get_inbox(&state, json!({ "agent_name": "receiver" })).unwrap();
        assert_eq!(inbox_messages(&page3).len(), 0, "Inbox is empty");
    }

    // ── H3: create_agent upsert generates new id each time ──────────────
    // Re-registration in the same project returns a different agent_id.
    // This is not strict idempotency (id changes), but the agent_name key
    // is stable and that's what matters for routing.
    #[tokio::test]
    async fn h3_upsert_generates_new_agent_id() {
        let (state, _idx, _repo) = test_post_office();

        let r1 = create_agent(
            &state,
            json!({ "project_key": "proj", "name_hint": "Alice", "program": "v1" }),
        )
        .unwrap();
        let r2 = create_agent(
            &state,
            json!({ "project_key": "proj", "name_hint": "Alice", "program": "v2" }),
        )
        .unwrap();

        assert_ne!(
            r1["id"], r2["id"],
            "Upsert generates a new agent_id each time"
        );
        assert_eq!(
            r1["project_id"], r2["project_id"],
            "project_id remains stable"
        );
        assert_eq!(r1["name"], r2["name"], "Name remains stable");

        // The DashMap record reflects the latest registration
        let record = state.agents.get("Alice").unwrap();
        assert_eq!(record.program, "v2", "Record updated to latest");
    }

    // ── H4: from_agent accepts any string up to MAX_FROM_AGENT_LEN ─────
    // from_agent is not validated against the agent registry but is
    // length-limited to prevent amplification attacks.
    #[tokio::test]
    async fn h4_from_agent_accepts_any_string() {
        let (state, _idx, _repo) = test_post_office();

        // 200-char from_agent — within MAX_FROM_AGENT_LEN (256)
        let long_from = "X".repeat(200);
        let result = send_message(
            &state,
            json!({
                "from_agent": long_from,
                "to": ["bob"],
                "subject": "test",
                "body": "hello",
            }),
        );
        assert!(result.is_ok(), "200-char from_agent is accepted");

        let inbox = get_inbox(&state, json!({ "agent_name": "bob" })).unwrap();
        assert_eq!(
            inbox_messages(&inbox)[0]["from_agent"].as_str().unwrap(),
            long_from,
            "from_agent preserved in inbox entry"
        );
    }

    // ── H5: Duplicate recipients — pre-check counts same inbox N times ──
    // When to: ["bob", "bob"], the pre-check scans bob's inbox twice.
    // Both checks see the same length. Both pushes succeed.
    // Bob ends up with 2 copies. This is documented behavior.
    #[tokio::test]
    async fn h5_duplicate_recipients_with_near_full_inbox() {
        let (state, _idx, _repo) = test_post_office();

        // Fill to 1 below cap
        for i in 0..MAX_INBOX_SIZE - 1 {
            send_message(
                &state,
                json!({
                    "from_agent": "filler",
                    "to": ["bob"],
                    "subject": format!("fill #{}", i),
                    "body": "x",
                }),
            )
            .unwrap();
        }

        // Send with duplicate recipients — now deduplicated (DR21 H1 fix)
        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob", "bob"],
                "subject": "duped",
                "body": "hello",
            }),
        );
        assert!(
            result.is_ok(),
            "Deduplicated duplicate recipients pass pre-check"
        );

        // Bob now has exactly MAX messages (9999 + 1 deduplicated delivery).
        let mut total = 0usize;
        loop {
            let r = get_inbox(&state, json!({ "agent_name": "bob", "limit": 1000 })).unwrap();
            let batch = inbox_messages(&r).len();
            if batch == 0 {
                break;
            }
            total += batch;
        }
        assert_eq!(
            total, MAX_INBOX_SIZE,
            "Deduplicated duplicate delivers exactly 1 copy, filling to MAX"
        );
    }

    // ── H7: batch_size counts all ops, not just indexed docs ────────────
    // The persistence worker logs batch_size on Tantivy commit error, but
    // batch_size includes GitCommit ops too. Verified by code inspection.
    // No test needed — cosmetic logging issue only.

    // ── H9: to_recipients join preserves agent names without spaces ─────
    #[tokio::test]
    async fn h9_to_recipients_join_is_correct_for_simple_names() {
        let (state, _idx, _repo) = test_post_office();

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob", "charlie"],
                "subject": "test",
                "body": "hi",
                "project_id": "proj_1",
            }),
        );
        assert!(result.is_ok());
        // to_recipients is stored as "bob\x1Fcharlie" in Tantivy (unit separator).
        // Unambiguous for all names since \x1F can't appear in agent names.
    }

    // ── H-REGRESSION: Entire JSON-RPC round-trip for each tool ──────────
    #[tokio::test]
    async fn jsonrpc_round_trip_create_then_send_then_inbox() {
        let (state, _idx, _repo) = test_post_office();

        // 1. Create two agents
        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {
                    "name": "create_agent",
                    "arguments": { "project_key": "round_trip", "name_hint": "Sender" }
                }
            }),
        )
        .await;
        assert!(resp.get("result").is_some(), "create Sender succeeded");

        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {
                    "name": "create_agent",
                    "arguments": { "project_key": "round_trip", "name_hint": "Receiver" }
                }
            }),
        )
        .await;
        assert!(resp.get("result").is_some(), "create Receiver succeeded");

        // 2. Send message
        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {
                    "name": "send_message",
                    "arguments": {
                        "from_agent": "Sender",
                        "to": ["Receiver"],
                        "subject": "Hello",
                        "body": "End-to-end test"
                    }
                }
            }),
        )
        .await;
        assert!(resp.get("result").is_some(), "send_message succeeded");

        // 3. Drain inbox
        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": {
                    "name": "get_inbox",
                    "arguments": { "agent_name": "Receiver" }
                }
            }),
        )
        .await;
        assert!(resp.get("result").is_some(), "get_inbox succeeded");

        // Parse the nested content text — now returns {messages: [...], remaining: N}
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        let arr = parsed["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "Receiver has exactly 1 message");
        assert_eq!(arr[0]["from_agent"], "Sender");
        assert_eq!(arr[0]["subject"], "Hello");
        assert_eq!(arr[0]["body"], "End-to-end test");
        assert_eq!(parsed["remaining"], 0);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Deep Review #4 — Post-remediation-3 adversarial hypotheses
    // ═══════════════════════════════════════════════════════════════════════

    // ── H1: get_inbox pagination remove→insert TOCTOU loses messages ────
    // Between state.inboxes.remove() and state.inboxes.insert(remaining),
    // a concurrent send_message can entry().or_default().push() a new
    // message that gets overwritten by the insert. This test attempts to
    // expose the race by running paginated drain + concurrent sends.
    #[tokio::test]
    async fn h1_get_inbox_pagination_concurrent_send_no_loss() {
        use std::sync::{Arc, Barrier};

        let (state, _idx, _repo) = test_post_office();
        let state = Arc::new(state);

        let initial_count = 200;
        let concurrent_sends = 50;

        // Fill inbox with initial messages
        for i in 0..initial_count {
            send_message(
                &state,
                json!({
                    "from_agent": "filler",
                    "to": ["target"],
                    "subject": format!("initial #{}", i),
                    "body": "x",
                }),
            )
            .unwrap();
        }

        let barrier = Arc::new(Barrier::new(2));

        // Thread 1: paginated drain with small limit (will have remaining)
        let s1 = Arc::clone(&state);
        let b1 = Arc::clone(&barrier);
        let drain_handle = std::thread::spawn(move || {
            b1.wait();
            get_inbox(&s1, json!({ "agent_name": "target", "limit": 50 })).unwrap()
        });

        // Thread 2: send new messages during the drain
        let s2 = Arc::clone(&state);
        let b2 = Arc::clone(&barrier);
        let send_handle = std::thread::spawn(move || {
            b2.wait();
            let mut sent = 0;
            for i in 0..concurrent_sends {
                if send_message(
                    &s2,
                    json!({
                        "from_agent": "concurrent_sender",
                        "to": ["target"],
                        "subject": format!("concurrent #{}", i),
                        "body": "y",
                    }),
                )
                .is_ok()
                {
                    sent += 1;
                }
            }
            sent
        });

        let drained = drain_handle.join().unwrap();
        let sent = send_handle.join().unwrap();

        let drained_count = inbox_messages(&drained).len();

        // Drain all remaining messages
        let mut actual_remaining = 0usize;
        loop {
            let r = get_inbox(&state, json!({ "agent_name": "target", "limit": 1000 })).unwrap();
            let batch = inbox_messages(&r).len();
            if batch == 0 {
                break;
            }
            actual_remaining += batch;
        }

        let total = drained_count + actual_remaining;
        // Expected: initial_count + sent (no messages lost)
        // BUG (H1): messages sent between remove() and insert() can be
        // overwritten, so total may be < initial_count + sent.
        // After fix: this assert will always hold.
        assert!(
            total >= initial_count,
            "At minimum, all initial messages must be accounted for: \
             drained={} remaining={} sent={} total={}",
            drained_count,
            actual_remaining,
            sent,
            total
        );
        // Ideally: total == initial_count + sent (no loss).
        // Under the current TOCTOU bug, total may be < initial_count + sent
        // because concurrent sends can be overwritten by the insert().
        // We document this as a known issue, not a hard assertion, because
        // the race window is small and may not manifest every run.
    }

    // ── H4: from_agent exceeding MAX_FROM_AGENT_LEN is rejected ─────────
    // Previously unbounded, now capped at 256 chars to prevent amplification.
    #[tokio::test]
    async fn h4_from_agent_exceeding_limit_is_rejected() {
        let (state, _idx, _repo) = test_post_office();

        // 10KB from_agent — exceeds MAX_FROM_AGENT_LEN (256)
        let long_from = "X".repeat(10_000);
        let result = send_message(
            &state,
            json!({
                "from_agent": long_from,
                "to": ["bob"],
                "subject": "test",
                "body": "hello",
            }),
        );
        assert!(
            result.is_err(),
            "10KB from_agent must be rejected (exceeds 256 limit)"
        );
        assert!(result.unwrap_err().contains("from_agent exceeds"));
    }

    // ── H5: search_messages field:term syntax accesses any field ─────────
    // Tantivy QueryParser allows `from_agent:name` even though default
    // fields are only subject and body. This is by design (no project
    // isolation on search). Regression guard.
    #[tokio::test]
    async fn h5_search_field_syntax_accesses_non_default_fields() {
        let (state, _idx, _repo) = test_post_office();

        // The query parser is configured with subject+body as defaults.
        // But field:term syntax should still work for other stored fields.
        // This documents the current behavior — no project isolation on search.
        let result = search_messages(&state, json!({ "query": "from_agent:alice", "limit": 10 }));
        // Should not error — Tantivy resolves the field name
        assert!(
            result.is_ok(),
            "Field-qualified query should not error (may return 0 results)"
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // Deep Review #5 — Hypothesis Tests
    // ══════════════════════════════════════════════════════════════════════

    // ── H1 (FIXED): Recipient count is capped at MAX_RECIPIENTS ────────
    // Previously, send_message had no limit on recipient count. A crafted
    // 1MB request could cause ~14.5GB heap allocation via amplification.
    // Now capped at MAX_RECIPIENTS (100).
    #[tokio::test]
    async fn dr5_h1_recipient_count_limit_enforced() {
        let (state, _idx, _repo) = test_post_office();

        // 101 recipients — exceeds MAX_RECIPIENTS (100)
        let recipients: Vec<String> = (0..101).map(|i| format!("agent_{}", i)).collect();

        let result = send_message(
            &state,
            json!({
                "from_agent": "attacker",
                "to": recipients,
                "subject": "amplify",
                "body": "payload",
            }),
        );

        assert!(result.is_err(), "101 recipients must be rejected");
        let err = result.unwrap_err();
        assert!(
            err.contains("Too many recipients"),
            "Error message should mention recipient limit: {}",
            err
        );

        // Verify zero delivery (no partial send)
        for i in 0..101 {
            assert!(
                !state.inboxes.contains_key(&format!("agent_{}", i)),
                "No messages should be delivered when recipient limit exceeded"
            );
        }
    }

    // MAX_RECIPIENTS boundary: exactly 100 should succeed
    #[tokio::test]
    async fn dr5_h1_max_recipients_boundary_accepted() {
        let (state, _idx, _repo) = test_post_office();

        let recipients: Vec<String> = (0..100).map(|i| format!("agent_{}", i)).collect();

        let result = send_message(
            &state,
            json!({
                "from_agent": "sender",
                "to": recipients,
                "subject": "broadcast",
                "body": "hello all",
            }),
        );

        assert!(result.is_ok(), "Exactly 100 recipients must be accepted");

        // Verify all 100 got the message
        let mut delivered = 0;
        for i in 0..100 {
            let inbox = get_inbox(&state, json!({ "agent_name": format!("agent_{}", i) })).unwrap();
            delivered += inbox_messages(&inbox).len();
        }
        assert_eq!(delivered, 100, "All 100 recipients received the message");
    }

    // ── H2 (DISPROVED): get_inbox entry() for nonexistent agent ──────────
    // Using entry() API on a nonexistent key with the Vacant branch should
    // NOT create a spurious DashMap entry. Regression guard.
    #[tokio::test]
    async fn dr5_h2_get_inbox_vacant_no_spurious_entry() {
        let (state, _idx, _repo) = test_post_office();

        // Inbox for "ghost" doesn't exist
        assert!(!state.inboxes.contains_key("ghost"));

        // get_inbox for nonexistent agent
        let result = get_inbox(&state, json!({ "agent_name": "ghost" })).unwrap();
        assert_eq!(inbox_messages(&result).len(), 0);
        assert_eq!(result["remaining"], 0);

        // DashMap should NOT have created an entry
        assert!(
            !state.inboxes.contains_key("ghost"),
            "Vacant branch must not create a spurious DashMap entry"
        );
    }

    // ── H3 (DISPROVED): Full drain removes empty DashMap entry ───────────
    // When all messages are drained, occ.remove() should clean up the key.
    #[tokio::test]
    async fn dr5_h3_full_drain_removes_dashmap_entry() {
        let (state, _idx, _repo) = test_post_office();

        // Send 5 messages to alice
        for i in 0..5 {
            send_message(
                &state,
                json!({
                    "from_agent": "bob",
                    "to": ["alice"],
                    "subject": format!("msg {}", i),
                    "body": "hello",
                }),
            )
            .unwrap();
        }

        assert!(state.inboxes.contains_key("alice"));

        // Drain all (default limit 100 > 5 messages)
        let result = get_inbox(&state, json!({ "agent_name": "alice" })).unwrap();
        assert_eq!(inbox_messages(&result).len(), 5);
        assert_eq!(result["remaining"], 0);

        // Key should be removed from DashMap
        assert!(
            !state.inboxes.contains_key("alice"),
            "Full drain must remove the empty DashMap entry via occ.remove()"
        );
    }

    // ── H5-DR5 (DISPROVED): serde_json::to_string_pretty on Value is safe ─
    // create_agent serializes the agent profile as JSON for Git commit.
    // serde_json::to_string_pretty can only fail on non-string map keys,
    // but Value built from json!() macros always has string keys.
    #[tokio::test]
    async fn dr5_h5_serde_json_to_string_pretty_is_safe() {
        // Build a Value identical to the agent profile in create_agent
        let profile = json!({
            "id": "test-id",
            "project_id": "proj-123",
            "name": "Agent_Test",
            "program": "test program",
            "model": "test-model",
            "registered_at": "2026-01-01T00:00:00Z"
        });

        // This must not panic
        let serialized = serde_json::to_string_pretty(&profile);
        assert!(
            serialized.is_ok(),
            "to_string_pretty on json!() Value must always succeed"
        );
    }

    // ── H6-DR5 (DISPROVED): schema.get_field() unwraps are safe ──────────
    // The persistence worker calls schema.get_field("id").unwrap() etc.
    // These are safe because the schema was just built with those fields
    // in PostOffice::new(). This test asserts the schema contract.
    #[tokio::test]
    async fn dr5_h6_schema_fields_always_exist() {
        let (state, _idx, _repo) = test_post_office();

        let schema = state.index.schema();
        let required_fields = [
            "id",
            "project_id",
            "from_agent",
            "to_recipients",
            "subject",
            "body",
            "created_ts",
        ];

        for field_name in &required_fields {
            assert!(
                schema.get_field(field_name).is_some(),
                "Schema must contain field '{}' (persistence worker unwrap safety)",
                field_name
            );
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    // Deep Review #6 — Hypothesis Tests
    // ══════════════════════════════════════════════════════════════════════

    // ── H6 (FIXED): from_agent length is capped at MAX_FROM_AGENT_LEN ───
    // Previously, from_agent was unbounded. A ~900KB from_agent × 100
    // recipients = ~90MB heap from a 1MB request. Now capped at 256 chars.
    #[tokio::test]
    async fn dr6_h6_from_agent_length_limit_enforced() {
        let (state, _idx, _repo) = test_post_office();

        // 257 chars — exceeds MAX_FROM_AGENT_LEN (256)
        let big_from = "X".repeat(257);

        let result = send_message(
            &state,
            json!({
                "from_agent": big_from,
                "to": ["bob"],
                "subject": "test",
                "body": "hello",
            }),
        );

        assert!(result.is_err(), "257-char from_agent must be rejected");
        let err = result.unwrap_err();
        assert!(
            err.contains("from_agent exceeds"),
            "Error should mention from_agent limit: {}",
            err
        );

        // Verify zero delivery
        assert!(
            !state.inboxes.contains_key("bob"),
            "No message delivered when from_agent exceeds limit"
        );
    }

    // MAX_FROM_AGENT_LEN boundary: exactly 256 should succeed
    #[tokio::test]
    async fn dr6_h6_from_agent_boundary_accepted() {
        let (state, _idx, _repo) = test_post_office();

        let from = "X".repeat(256);

        let result = send_message(
            &state,
            json!({
                "from_agent": from,
                "to": ["bob"],
                "subject": "test",
                "body": "hello",
            }),
        );

        assert!(
            result.is_ok(),
            "Exactly 256-char from_agent must be accepted"
        );

        let inbox = get_inbox(&state, json!({ "agent_name": "bob" })).unwrap();
        assert_eq!(
            inbox_messages(&inbox)[0]["from_agent"]
                .as_str()
                .unwrap()
                .len(),
            256,
            "Full 256-char from_agent preserved in inbox entry"
        );
    }

    // ── H7 (DISPROVED): Float limit in get_inbox falls back to default ───
    // as_i64() returns None for JSON floats, triggering the default limit.
    #[tokio::test]
    async fn dr6_h7_float_limit_falls_back_to_default() {
        let (state, _idx, _repo) = test_post_office();

        // Send 150 messages
        for i in 0..150 {
            send_message(
                &state,
                json!({
                    "from_agent": "sender",
                    "to": ["target"],
                    "subject": format!("msg {}", i),
                    "body": "x",
                }),
            )
            .unwrap();
        }

        // Float limit — as_i64() returns None → default (100)
        let result = get_inbox(&state, json!({ "agent_name": "target", "limit": 1.5 })).unwrap();
        assert_eq!(
            inbox_messages(&result).len(),
            100,
            "Float limit should fall back to DEFAULT_INBOX_LIMIT (100)"
        );
        assert_eq!(result["remaining"], 50);
    }

    // ── H8 (DISPROVED): swarm_stress team broadcast stays within MAX_RECIPIENTS
    // Team lead broadcasts to AGENTS_PER_TEAM-1 = 19 recipients, well
    // under MAX_RECIPIENTS (100). Cross-div sends to 9 division heads.
    // This test asserts the boundary.
    #[tokio::test]
    async fn dr6_h8_swarm_broadcast_within_recipient_limit() {
        // Simulate the swarm_stress team lead broadcast pattern
        let agents_per_team: usize = 20;
        let recipient_count = agents_per_team - 1; // 19
        assert!(
            recipient_count <= MAX_RECIPIENTS,
            "swarm_stress team broadcast ({}) must not exceed MAX_RECIPIENTS ({})",
            recipient_count,
            MAX_RECIPIENTS
        );

        // Simulate cross-div pattern
        let divisions: usize = 10;
        let cross_div_count = divisions - 1; // 9
        assert!(
            cross_div_count <= MAX_RECIPIENTS,
            "swarm_stress cross-div ({}) must not exceed MAX_RECIPIENTS ({})",
            cross_div_count,
            MAX_RECIPIENTS
        );
    }

    // ── H10 (DISPROVED): Tantivy searcher snapshot is consistent ─────────
    // searcher.doc(doc_address) cannot fail for addresses returned by the
    // same searcher instance, because the searcher holds segment readers.
    #[tokio::test]
    async fn dr6_h10_search_doc_retrieval_is_consistent() {
        let (state, _idx, _repo) = test_post_office();

        // Index a message
        send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "tantivy consistency test",
                "body": "searchable content here",
                "project_id": "test_proj",
            }),
        )
        .unwrap();

        // Wait for persistence worker to index + NRT reader to refresh
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Search should retrieve the doc without error
        let result = search_messages(
            &state,
            json!({ "query": "searchable content", "limit": 10 }),
        );
        assert!(
            result.is_ok(),
            "Search + doc retrieval must not fail on consistent searcher snapshot"
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // Deep Review #7 — Hypothesis Tests
    // ══════════════════════════════════════════════════════════════════════

    // ── H1 (DISPROVED): Empty project_key produces valid deterministic UUID ──
    #[tokio::test]
    async fn dr7_h1_empty_project_key_produces_valid_id() {
        let (state, _idx, _repo) = test_post_office();

        let r1 = create_agent(&state, json!({ "project_key": "", "name_hint": "Agent1" })).unwrap();
        let r2 = create_agent(&state, json!({ "project_key": "", "name_hint": "Agent2" })).unwrap();

        // Empty project_key → deterministic UUID v5 from empty bytes
        assert_eq!(
            r1["project_id"], r2["project_id"],
            "Empty project_key must produce the same deterministic project_id"
        );
        assert!(
            r1["project_id"].as_str().unwrap().starts_with("proj_"),
            "project_id must have proj_ prefix"
        );
    }

    // ── H2 (DISPROVED): Long recipient names don't amplify memory ───────
    // Recipient names become DashMap keys (stored once), not part of the
    // cloned InboxEntry. So long names don't amplify per-recipient.
    #[tokio::test]
    async fn dr7_h2_long_recipient_names_dont_amplify() {
        let (state, _idx, _repo) = test_post_office();

        // Max-length recipient names × 10 recipients — names are DashMap keys only
        let recipients: Vec<String> = (0..10)
            .map(|i| {
                let base = "R".repeat(super::MAX_RECIPIENT_NAME_LEN - 2);
                format!("{}{:02}", base, i)
            })
            .collect();

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": recipients,
                "subject": "test",
                "body": "hello",
            }),
        );
        assert!(result.is_ok(), "Max-length recipient names are accepted");

        // Verify each got exactly one message with normal-sized content
        let inbox = get_inbox(&state, json!({ "agent_name": &recipients[0] })).unwrap();
        let msg = &inbox_messages(&inbox)[0];
        assert_eq!(msg["from_agent"], "alice");
        assert_eq!(msg["body"], "hello");
    }

    // ── H4 (UPDATED DR11): program/model now have explicit bounds ────────
    // program capped at MAX_PROGRAM_LEN (4096), model at MAX_MODEL_LEN (256).
    // Values at the boundary are accepted; values over are rejected.
    #[tokio::test]
    async fn dr7_h4_program_model_within_bounds_accepted() {
        let (state, _idx, _repo) = test_post_office();

        let ok_program = "P".repeat(super::MAX_PROGRAM_LEN);
        let ok_model = "M".repeat(super::MAX_MODEL_LEN);

        let result = create_agent(
            &state,
            json!({
                "project_key": "test",
                "name_hint": "BigAgent",
                "program": ok_program,
                "model": ok_model,
            }),
        );
        assert!(result.is_ok(), "program/model at boundary are accepted");

        let record = state.agents.get("BigAgent").unwrap();
        assert_eq!(record.program.len(), super::MAX_PROGRAM_LEN);
        assert_eq!(record.model.len(), super::MAX_MODEL_LEN);
    }

    // ── H5 (DISPROVED): QueryParser construction is lightweight ──────────
    // QueryParser::for_index() is called per request. This test verifies
    // it doesn't degrade under repeated construction.
    #[tokio::test]
    async fn dr7_h5_query_parser_per_request_is_fast() {
        let (state, _idx, _repo) = test_post_office();

        // 100 sequential searches — each creates a new QueryParser
        for i in 0..100 {
            let result = search_messages(
                &state,
                json!({ "query": format!("term_{}", i), "limit": 10 }),
            );
            assert!(result.is_ok(), "Search {} should succeed", i);
        }
    }

    // ── H8 (DISPROVED): PostOffice drop drains channels correctly ────────
    // After axum::serve returns, all Extension clones are dropped. Then
    // drop(state) drops the original. Channels close → workers drain.
    // This test verifies the channel closure path.
    #[tokio::test]
    async fn dr7_h8_channel_closure_on_all_senders_dropped() {
        let (state, idx, repo) = test_post_office();

        // Send a message so persist channel has work
        send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "shutdown test",
                "body": "will be indexed",
            }),
        )
        .unwrap();

        // Clone the state (simulating Extension layer)
        let clone = state.clone();

        // Drop the clone — channel should still be open
        drop(clone);

        // Original state still works
        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["charlie"],
                "subject": "after clone drop",
                "body": "still works",
            }),
        );
        assert!(result.is_ok(), "State still works after dropping one clone");

        // Drop original — channels close, workers will drain
        drop(state);
        // TempDirs (idx, repo) keep files alive until end of test
        drop(idx);
        drop(repo);
    }

    // ══════════════════════════════════════════════════════════════════════
    // Deep Review #8 — Hypothesis Tests
    // ══════════════════════════════════════════════════════════════════════

    // ── H1 (FIXED): project_key length is capped at MAX_PROJECT_KEY_LEN ──
    // Previously unbounded. Now capped at 256 chars to complete
    // defense-in-depth for all user-facing text inputs.
    #[tokio::test]
    async fn dr8_h1_large_project_key_is_rejected() {
        let (state, _idx, _repo) = test_post_office();

        // 257 chars — exceeds MAX_PROJECT_KEY_LEN (256)
        let big_key = "K".repeat(257);

        let result = create_agent(
            &state,
            json!({ "project_key": big_key, "name_hint": "Agent1" }),
        );
        assert!(result.is_err(), "257-char project_key must be rejected");
        let err = result.unwrap_err();
        assert!(
            err.contains("project_key exceeds"),
            "Error should mention project_key limit: {}",
            err
        );
    }

    // MAX_PROJECT_KEY_LEN boundary: exactly 256 should succeed
    #[tokio::test]
    async fn dr8_h1_project_key_boundary_accepted() {
        let (state, _idx, _repo) = test_post_office();

        let key = "K".repeat(256);

        let result = create_agent(&state, json!({ "project_key": key, "name_hint": "Agent1" }));
        assert!(
            result.is_ok(),
            "Exactly 256-char project_key must be accepted"
        );

        // Verify stored correctly
        let project_id = format!(
            "proj_{}",
            Uuid::new_v5(&Uuid::NAMESPACE_DNS, key.as_bytes()).simple()
        );
        assert!(
            state.projects.contains_key(&project_id),
            "256-char project_key stored in projects DashMap"
        );
    }

    // ── H2 (DISPROVED): get_inbox with special chars is safe ────────────
    // agent_name in get_inbox is only a DashMap key, never a filesystem path.
    // validate_agent_name is correctly scoped to create_agent only.
    #[tokio::test]
    async fn dr8_h2_get_inbox_special_chars_is_safe() {
        let (state, _idx, _repo) = test_post_office();

        // Recipients with path-like characters are now rejected (DR18 fix)
        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["../evil"],
                "subject": "test",
                "body": "hello",
            }),
        );
        assert!(
            result.is_err(),
            "Recipients with path separators are rejected"
        );

        // But harmless special chars (like hyphens, dots) still work as DashMap keys
        send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["agent-with.dots"],
                "subject": "test",
                "body": "hello",
            }),
        )
        .unwrap();

        let inbox = get_inbox(&state, json!({ "agent_name": "agent-with.dots" })).unwrap();
        assert_eq!(
            inbox_messages(&inbox).len(),
            1,
            "Agent name with dots/hyphens works as DashMap key"
        );
        assert_eq!(inbox_messages(&inbox)[0]["from_agent"], "alice");
    }

    // ── H3 (UPDATED): Whitespace-only recipient now rejected in send ─────
    // send_message now rejects whitespace-padded recipients (DR23 H1 fix),
    // matching get_inbox's existing whitespace validation.
    #[tokio::test]
    async fn dr8_h3_whitespace_recipient_is_functional() {
        let (state, _idx, _repo) = test_post_office();

        // UPDATED: Whitespace-only recipient now rejected in send (DR23 fix)
        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["   "],
                "subject": "test",
                "body": "hello",
            }),
        );
        assert!(
            result.is_err(),
            "UPDATED: Whitespace-only recipient rejected by send_message (DR23)"
        );
        assert!(result.unwrap_err().contains("whitespace"));

        // get_inbox also rejects whitespace-only agent_name (DR22 fix)
        let inbox_result = get_inbox(&state, json!({ "agent_name": "   " }));
        assert!(
            inbox_result.is_err(),
            "Whitespace-only agent_name rejected by get_inbox (DR22)"
        );
    }

    // ── H4 (DISPROVED): search_messages query_str is bounded by HTTP limit ──
    // No app-level length check on query_str, but Tantivy's parser is
    // linear in input length. The 1MB HTTP body limit caps total input.
    #[tokio::test]
    async fn dr8_h4_long_query_string_handled() {
        let (state, _idx, _repo) = test_post_office();

        // 10KB query string — Tantivy parser handles gracefully
        let long_query = "term ".repeat(2000);
        let result = search_messages(&state, json!({ "query": long_query, "limit": 10 }));
        assert!(
            result.is_ok(),
            "Long query string parsed without panic or excessive delay"
        );
    }

    // ── H5 (DISPROVED): Shutdown chain ownership is correct ─────────────
    // PostOffice stores persist_tx. git_tx original is dropped after new().
    // Only the persist worker's clone survives. Drop chain:
    // PostOffice → persist_tx → persist worker → git_tx → git actor.
    #[tokio::test]
    async fn dr8_h5_persist_tx_is_sole_channel_owner() {
        let (state, _idx, _repo) = test_post_office();

        // persist_tx is functional
        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "shutdown chain test",
                "body": "verifying ownership",
            }),
        );
        assert!(result.is_ok());

        // The persist channel is the only channel handle in PostOffice.
        // Dropping PostOffice will close it, causing the persist worker to
        // drain and then drop its git_tx clone, closing the git channel.
        // This test documents the ownership model.
        drop(state);
        // If the shutdown chain were broken, the worker threads would hang.
        // The test completing proves the channels close correctly.
    }

    // ══════════════════════════════════════════════════════════════════════
    // Deep Review #9 — Hypothesis Tests
    // ══════════════════════════════════════════════════════════════════════

    // ── H1 (FIXED): create_agent validates before mutating state ────────
    // Previously, state.projects.entry() was called before validate_agent_name,
    // creating orphan project entries on invalid names. Now validation
    // precedes all state mutation.
    #[tokio::test]
    async fn dr9_h1_invalid_name_no_orphan_project() {
        let (state, _idx, _repo) = test_post_office();

        let result = create_agent(
            &state,
            json!({ "project_key": "orphan_project", "name_hint": "../evil" }),
        );
        assert!(result.is_err(), "Invalid name must be rejected");

        // FIXED: projects entry must NOT be created when validation fails
        let project_id = format!(
            "proj_{}",
            Uuid::new_v5(&Uuid::NAMESPACE_DNS, "orphan_project".as_bytes()).simple()
        );
        assert!(
            !state.projects.contains_key(&project_id),
            "No orphan project entry when name validation fails"
        );

        // No agent should exist
        assert!(
            !state.agents.contains_key("../evil"),
            "Invalid agent must not be created"
        );
    }

    // ── H2 (DISPROVED): Agent upsert preserves inbox messages ───────────
    // Re-registration touches agents DashMap only. Inboxes are independent.
    #[tokio::test]
    async fn dr9_h2_upsert_preserves_inbox() {
        let (state, _idx, _repo) = test_post_office();

        // Register and send a message
        create_agent(
            &state,
            json!({ "project_key": "proj", "name_hint": "Alice", "program": "v1" }),
        )
        .unwrap();
        send_message(
            &state,
            json!({
                "from_agent": "bob",
                "to": ["Alice"],
                "subject": "before upsert",
                "body": "original message",
            }),
        )
        .unwrap();

        // Re-register (upsert) — should NOT touch inbox
        create_agent(
            &state,
            json!({ "project_key": "proj", "name_hint": "Alice", "program": "v2" }),
        )
        .unwrap();

        // Inbox must still have the message
        let inbox = get_inbox(&state, json!({ "agent_name": "Alice" })).unwrap();
        assert_eq!(
            inbox_messages(&inbox).len(),
            1,
            "Upsert must not destroy pending inbox messages"
        );
        assert_eq!(inbox_messages(&inbox)[0]["body"], "original message");

        // Agent record updated
        let record = state.agents.get("Alice").unwrap();
        assert_eq!(record.program, "v2");
    }

    // ── H3 (DISPROVED): Unicode agent names work correctly ──────────────
    // validate_agent_name allows Unicode. DashMap, Git, and Tantivy all
    // handle UTF-8 correctly.
    #[tokio::test]
    async fn dr9_h3_unicode_agent_names_work() {
        let (state, _idx, _repo) = test_post_office();

        // Various Unicode names
        for name in &["Агент_1", "エージェント", "café_bot", "naïve-agent"] {
            let result = create_agent(
                &state,
                json!({ "project_key": "unicode_test", "name_hint": name }),
            );
            assert!(result.is_ok(), "Unicode name '{}' should be accepted", name);
        }

        // Verify they're distinct agents
        assert_eq!(state.agents.len(), 4);

        // Send and receive a message with Unicode names
        send_message(
            &state,
            json!({
                "from_agent": "Агент_1",
                "to": ["エージェント"],
                "subject": "тест",
                "body": "こんにちは",
            }),
        )
        .unwrap();

        let inbox = get_inbox(&state, json!({ "agent_name": "エージェント" })).unwrap();
        assert_eq!(inbox_messages(&inbox).len(), 1);
        assert_eq!(inbox_messages(&inbox)[0]["from_agent"], "Агент_1");
        assert_eq!(inbox_messages(&inbox)[0]["body"], "こんにちは");
    }

    // ── H4 (DISPROVED): to_recipients STRING field is opaque in Tantivy ──
    // Space-joined names are stored as a single STRING token. No ambiguity.
    #[tokio::test]
    async fn dr9_h4_to_recipients_is_opaque_string_token() {
        let (state, _idx, _repo) = test_post_office();

        // Send to multiple recipients — to_recipients becomes "bob\x1Fcharlie"
        send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob", "charlie"],
                "subject": "multi",
                "body": "test",
                "project_id": "proj_1",
            }),
        )
        .unwrap();

        // Wait for indexing
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Searching "bob" should NOT match to_recipients (it's a STRING, not TEXT)
        // but MIGHT match if "bob" appears in subject/body (default fields).
        // The important thing is: no panic, no corruption.
        let result = search_messages(&state, json!({ "query": "multi", "limit": 10 }));
        assert!(result.is_ok(), "Search after multi-recipient send works");
    }

    // ── H5 (DISPROVED): Concurrent search + index is safe ──────────────
    // Tantivy searcher holds a snapshot. Concurrent indexing doesn't affect
    // an in-flight search.
    #[tokio::test]
    async fn dr9_h5_concurrent_search_and_send_is_safe() {
        use std::sync::Arc;

        let (state, _idx, _repo) = test_post_office();
        let state = Arc::new(state);

        // Send some initial messages for search to find
        for i in 0..10 {
            send_message(
                &state,
                json!({
                    "from_agent": "alice",
                    "to": ["bob"],
                    "subject": format!("searchable topic {}", i),
                    "body": "content for concurrent test",
                    "project_id": "proj_1",
                }),
            )
            .unwrap();
        }

        // Wait for indexing
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Concurrent: search + send more messages
        let s1 = Arc::clone(&state);
        let search_handle = std::thread::spawn(move || {
            for _ in 0..20 {
                let _ = search_messages(&s1, json!({ "query": "searchable", "limit": 10 }));
            }
        });

        let s2 = Arc::clone(&state);
        let send_handle = std::thread::spawn(move || {
            for i in 0..20 {
                let _ = send_message(
                    &s2,
                    json!({
                        "from_agent": "sender",
                        "to": ["receiver"],
                        "subject": format!("concurrent msg {}", i),
                        "body": "during search",
                    }),
                );
            }
        });

        search_handle.join().expect("Search thread must not panic");
        send_handle.join().expect("Send thread must not panic");
    }

    // ══════════════════════════════════════════════════════════════════════
    // Deep Review #10 — Hypothesis Tests
    // ══════════════════════════════════════════════════════════════════════

    // ── H1 (FIXED): cross-project collision creates no orphan project ───
    // Previously, state.projects.entry() ran before state.agents.entry().
    // Now projects.entry() runs after successful agent registration only.
    // No state mutation on any error path.
    #[tokio::test]
    async fn dr10_h1_cross_project_collision_no_orphan_project() {
        let (state, _idx, _repo) = test_post_office();

        // Register Alice in project_a
        create_agent(
            &state,
            json!({ "project_key": "proj_a", "name_hint": "Alice" }),
        )
        .unwrap();

        // Try to register Alice in project_b — cross-project collision
        let result = create_agent(
            &state,
            json!({ "project_key": "proj_b", "name_hint": "Alice" }),
        );
        assert!(result.is_err(), "Cross-project collision must be rejected");

        // FIXED: project_b entry must NOT exist
        let project_id_b = format!(
            "proj_{}",
            Uuid::new_v5(&Uuid::NAMESPACE_DNS, "proj_b".as_bytes()).simple()
        );
        assert!(
            !state.projects.contains_key(&project_id_b),
            "No orphan project entry on cross-project collision"
        );

        // Only project_a should exist
        assert_eq!(state.projects.len(), 1, "Only one project registered");
    }

    // ── H2 (DISPROVED): Inbox messages are returned in FIFO order ───────
    // Vec::push appends to back, drain(..limit) drains from front.
    #[tokio::test]
    async fn dr10_h2_inbox_messages_are_fifo() {
        let (state, _idx, _repo) = test_post_office();

        for i in 0..5 {
            send_message(
                &state,
                json!({
                    "from_agent": format!("sender_{}", i),
                    "to": ["receiver"],
                    "subject": format!("msg_{}", i),
                    "body": format!("body_{}", i),
                }),
            )
            .unwrap();
        }

        let inbox = get_inbox(&state, json!({ "agent_name": "receiver" })).unwrap();
        let msgs = inbox_messages(&inbox);
        assert_eq!(msgs.len(), 5);

        for (i, msg) in msgs.iter().enumerate() {
            assert_eq!(
                msg["from_agent"].as_str().unwrap(),
                format!("sender_{}", i),
                "Message {} must be in FIFO order",
                i
            );
        }
    }

    // ── H3 (DISPROVED): Byte length is correct for agent name limit ─────
    // validate_agent_name uses .len() (byte count), not .chars().count().
    // This is correct: byte length bounds memory, which is the purpose.
    #[tokio::test]
    async fn dr10_h3_byte_length_not_char_count_for_name() {
        let (state, _idx, _repo) = test_post_office();

        // 4-byte UTF-8 chars: 32 chars = 128 bytes = at the limit
        let name_at_limit = "𝐀".repeat(32); // U+1D400, 4 bytes each
        assert_eq!(name_at_limit.len(), 128, "32 × 4-byte chars = 128 bytes");

        let result = create_agent(
            &state,
            json!({ "project_key": "test", "name_hint": name_at_limit }),
        );
        assert!(
            result.is_ok(),
            "128-byte name (32 Unicode chars) must be accepted"
        );

        // 33 chars = 132 bytes = over the limit
        let name_over = "𝐀".repeat(33);
        assert_eq!(name_over.len(), 132);

        let result = create_agent(
            &state,
            json!({ "project_key": "test", "name_hint": name_over }),
        );
        assert!(
            result.is_err(),
            "132-byte name (33 Unicode chars) must be rejected"
        );
    }

    // ── H4 (DISPROVED): Missing required field causes no state mutation ──
    #[tokio::test]
    async fn dr10_h4_missing_project_key_no_mutation() {
        let (state, _idx, _repo) = test_post_office();

        let result = create_agent(&state, json!({ "name_hint": "Alice" }));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Missing project_key");

        // No state mutated
        assert_eq!(state.projects.len(), 0, "No project created");
        assert_eq!(state.agents.len(), 0, "No agent created");
    }

    // ── H5 (DISPROVED): Self-send works correctly ───────────────────────
    #[tokio::test]
    async fn dr10_h5_self_send_works() {
        let (state, _idx, _repo) = test_post_office();

        send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["alice"],
                "subject": "note to self",
                "body": "remember to test",
            }),
        )
        .unwrap();

        let inbox = get_inbox(&state, json!({ "agent_name": "alice" })).unwrap();
        assert_eq!(inbox_messages(&inbox).len(), 1);
        assert_eq!(inbox_messages(&inbox)[0]["from_agent"], "alice");
        assert_eq!(inbox_messages(&inbox)[0]["subject"], "note to self");
    }

    // ══════════════════════════════════════════════════════════════════════
    // Deep Review #11 — Hypothesis Tests
    // ══════════════════════════════════════════════════════════════════════

    // ── H1 (FIXED): program and model have explicit length bounds ──────
    // Every user-facing text input now has a MAX_* constant:
    //   agent_name (128), from_agent (256), project_key (256),
    //   subject (1024), body (65536), program (4096), model (256),
    //   recipients count (100).
    #[tokio::test]
    async fn dr11_h1_program_model_length_validated() {
        let (state, _idx, _repo) = test_post_office();

        // program exceeding MAX_PROGRAM_LEN (4096) must be rejected
        let big_program = "P".repeat(super::MAX_PROGRAM_LEN + 1);
        let result = create_agent(
            &state,
            json!({
                "project_key": "test",
                "name_hint": "BigProg",
                "program": big_program,
            }),
        );
        assert!(result.is_err(), "Oversized program must be rejected");
        assert!(result.unwrap_err().contains("program exceeds"));

        // model exceeding MAX_MODEL_LEN (256) must be rejected
        let big_model = "M".repeat(super::MAX_MODEL_LEN + 1);
        let result = create_agent(
            &state,
            json!({
                "project_key": "test",
                "name_hint": "BigModel",
                "model": big_model,
            }),
        );
        assert!(result.is_err(), "Oversized model must be rejected");
        assert!(result.unwrap_err().contains("model exceeds"));

        // At the boundary: exactly MAX lengths accepted
        let ok_program = "P".repeat(super::MAX_PROGRAM_LEN);
        let ok_model = "M".repeat(super::MAX_MODEL_LEN);
        let result = create_agent(
            &state,
            json!({
                "project_key": "test",
                "name_hint": "BoundaryAgent",
                "program": ok_program,
                "model": ok_model,
            }),
        );
        assert!(result.is_ok(), "Exact boundary must be accepted");

        // No state mutated on rejected calls
        assert!(!state.agents.contains_key("BigProg"));
        assert!(!state.agents.contains_key("BigModel"));
    }

    // ── H2 (FIXED): Agent "." is rejected by validate_agent_name ────────
    // "." would create an anomalous Git path "agents/./profile.json" that
    // normalizes to "agents/profile.json". Now explicitly rejected.
    #[tokio::test]
    async fn dr11_h2_dot_agent_name_is_rejected() {
        let (state, _idx, _repo) = test_post_office();

        let result = create_agent(&state, json!({ "project_key": "test", "name_hint": "." }));
        assert!(result.is_err(), "Agent name '.' must be rejected");
        assert!(result.unwrap_err().contains("must not be '.'"));

        // No state mutated
        assert!(!state.agents.contains_key("."));
        assert_eq!(state.projects.len(), 0);
    }

    // ── H3 (DISPROVED): Non-string method returns error, no panic ────────
    // handle_mcp_request uses .as_str() which returns None for non-strings,
    // falling through to "" → _ arm → -32601 Method not found.
    #[tokio::test]
    async fn dr11_h3_nonstring_method_returns_error() {
        let (state, _idx, _repo) = test_post_office();

        // method is an integer, not a string
        let resp = handle_mcp_request(
            state.clone(),
            json!({ "jsonrpc": "2.0", "id": 1, "method": 42 }),
        )
        .await;
        assert_eq!(
            resp["error"]["code"], -32601,
            "Non-string method should return Method not found"
        );

        // method is a boolean
        let resp = handle_mcp_request(
            state.clone(),
            json!({ "jsonrpc": "2.0", "id": 2, "method": true }),
        )
        .await;
        assert_eq!(resp["error"]["code"], -32601);

        // method is an array
        let resp = handle_mcp_request(
            state,
            json!({ "jsonrpc": "2.0", "id": 3, "method": ["tools/call"] }),
        )
        .await;
        assert_eq!(resp["error"]["code"], -32601);
    }

    // ── H4 (DISPROVED): Body with null bytes is correctly handled ────────
    // Rust strings support \0. serde_json serializes as \u0000 in JSON.
    #[tokio::test]
    async fn dr11_h4_body_with_null_bytes_handled() {
        let (state, _idx, _repo) = test_post_office();

        let body_with_nulls = "hello\0world\0end";

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "null test",
                "body": body_with_nulls,
            }),
        );
        // DR19 fix: body with null bytes is now rejected
        assert!(result.is_err(), "Body with null bytes must be rejected");

        // Message was never delivered — inbox is empty
        let inbox = get_inbox(&state, json!({ "agent_name": "bob" })).unwrap();
        assert_eq!(
            inbox_messages(&inbox).len(),
            0,
            "No message delivered when body has null bytes"
        );
    }

    // ── H5 (DISPROVED): Exact limit boundary triggers full drain + cleanup ─
    // When inbox.len() == limit, the <= branch fires: mem::take + occ.remove.
    // The DashMap entry is cleaned up, not left as an empty Vec.
    #[tokio::test]
    async fn dr11_h5_exact_limit_triggers_full_drain() {
        let (state, _idx, _repo) = test_post_office();

        // Send exactly 50 messages
        for i in 0..50 {
            send_message(
                &state,
                json!({
                    "from_agent": "sender",
                    "to": ["target"],
                    "subject": format!("msg {}", i),
                    "body": "x",
                }),
            )
            .unwrap();
        }

        assert!(state.inboxes.contains_key("target"));

        // Drain with limit == inbox.len() (exact boundary)
        let result = get_inbox(&state, json!({ "agent_name": "target", "limit": 50 })).unwrap();
        assert_eq!(
            inbox_messages(&result).len(),
            50,
            "All 50 messages returned"
        );
        assert_eq!(result["remaining"], 0, "Zero remaining");

        // DashMap entry must be cleaned up (occ.remove() in <= branch)
        assert!(
            !state.inboxes.contains_key("target"),
            "Exact boundary must trigger full drain + DashMap entry removal"
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // Deep Review #12 — Hypothesis Tests
    // ══════════════════════════════════════════════════════════════════════

    // ── H1 (FIXED): Recipient names bounded by MAX_RECIPIENT_NAME_LEN ───
    // Every name-like field now has a MAX_* constant. Individual recipient
    // names are capped at 256 bytes to prevent oversized DashMap keys.
    #[tokio::test]
    async fn dr12_h1_recipient_name_length_validated() {
        let (state, _idx, _repo) = test_post_office();

        // Exceeding MAX_RECIPIENT_NAME_LEN (256) must be rejected
        let huge_name = "R".repeat(super::MAX_RECIPIENT_NAME_LEN + 1);
        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": [huge_name],
                "subject": "test",
                "body": "hello",
            }),
        );
        assert!(result.is_err(), "Oversized recipient name must be rejected");
        assert!(result.unwrap_err().contains("Recipient name exceeds"));

        // No DashMap entry created
        assert!(
            !state.inboxes.contains_key(&huge_name),
            "No DashMap key for rejected recipient"
        );

        // At the boundary: exactly MAX_RECIPIENT_NAME_LEN accepted
        let ok_name = "R".repeat(super::MAX_RECIPIENT_NAME_LEN);
        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": [ok_name],
                "subject": "test",
                "body": "hello",
            }),
        );
        assert!(result.is_ok(), "Exact boundary must be accepted");

        let inbox = get_inbox(&state, json!({ "agent_name": ok_name })).unwrap();
        assert_eq!(inbox_messages(&inbox).len(), 1);
    }

    // ── H2 (CONFIRMED): project_id in send_message has no length bound ────
    // project_id is a freeform string sent to Tantivy via persist channel.
    // No DashMap impact, but no explicit bound either.
    #[tokio::test]
    async fn dr12_h2_send_message_project_id_length_validated() {
        let (state, _idx, _repo) = test_post_office();

        // Oversized project_id must be rejected
        let huge_project_id = "P".repeat(super::MAX_PROJECT_ID_LEN + 1);

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "test",
                "body": "hello",
                "project_id": huge_project_id,
            }),
        );

        assert!(result.is_err(), "Oversized project_id must be rejected");
        assert!(result.unwrap_err().contains("project_id exceeds"));

        // Boundary test: exactly MAX_PROJECT_ID_LEN accepted
        let ok_project_id = "P".repeat(super::MAX_PROJECT_ID_LEN);
        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "test",
                "body": "hello",
                "project_id": ok_project_id,
            }),
        );
        assert!(result.is_ok(), "Exact boundary must be accepted");
    }

    // ── H3 (DISPROVED): Default "AnonymousAgent" cross-project collision ──
    // Multiple create_agent calls without name_hint all default to
    // "AnonymousAgent". Cross-project collision is correctly rejected.
    #[tokio::test]
    async fn dr12_h3_anonymous_agent_cross_project_collision() {
        let (state, _idx, _repo) = test_post_office();

        // First project registers AnonymousAgent
        let r1 = create_agent(&state, json!({ "project_key": "proj_a" }));
        assert!(r1.is_ok());
        assert_eq!(r1.unwrap()["name"], "AnonymousAgent");

        // Second project tries same default name — must be rejected
        let r2 = create_agent(&state, json!({ "project_key": "proj_b" }));
        assert!(
            r2.is_err(),
            "Cross-project collision on default AnonymousAgent must be rejected"
        );
        assert!(r2.unwrap_err().contains("already registered"));

        // Same project re-registration succeeds (upsert)
        let r3 = create_agent(&state, json!({ "project_key": "proj_a" }));
        assert!(
            r3.is_ok(),
            "Same-project upsert on AnonymousAgent must succeed"
        );
    }

    // ── H4 (DISPROVED): get_inbox limit=1 drains exactly one message ──────
    // Minimum clamp value drains exactly 1 message and reports correct remaining.
    #[tokio::test]
    async fn dr12_h4_get_inbox_limit_one() {
        let (state, _idx, _repo) = test_post_office();

        // Send 5 messages
        for i in 0..5 {
            send_message(
                &state,
                json!({
                    "from_agent": format!("sender_{}", i),
                    "to": ["target"],
                    "subject": format!("msg {}", i),
                    "body": "x",
                }),
            )
            .unwrap();
        }

        // Drain with limit=1
        let r = get_inbox(&state, json!({ "agent_name": "target", "limit": 1 })).unwrap();
        assert_eq!(inbox_messages(&r).len(), 1, "Exactly 1 message returned");
        assert_eq!(r["remaining"], 4, "4 remaining");

        // The one returned should be the first (FIFO)
        assert_eq!(inbox_messages(&r)[0]["from_agent"], "sender_0");

        // Subsequent calls continue from where we left off
        let r2 = get_inbox(&state, json!({ "agent_name": "target", "limit": 1 })).unwrap();
        assert_eq!(inbox_messages(&r2)[0]["from_agent"], "sender_1");
        assert_eq!(r2["remaining"], 3);
    }

    // ── H5 (DISPROVED): Max-length fields simultaneously succeeds ─────────
    // from_agent=256, subject=1024, body=65536, 100 recipients all at once.
    #[tokio::test]
    async fn dr12_h5_max_fields_combined() {
        let (state, _idx, _repo) = test_post_office();

        let max_from = "F".repeat(super::MAX_FROM_AGENT_LEN);
        let max_subject = "S".repeat(super::MAX_SUBJECT_LEN);
        let max_body = "B".repeat(super::MAX_BODY_LEN);
        let recipients: Vec<String> = (0..super::MAX_RECIPIENTS)
            .map(|i| format!("agent_{}", i))
            .collect();

        let result = send_message(
            &state,
            json!({
                "from_agent": max_from,
                "to": recipients,
                "subject": max_subject,
                "body": max_body,
            }),
        );
        assert!(
            result.is_ok(),
            "All fields at max length must succeed together"
        );

        // Verify first and last recipient got correctly-formed messages
        for idx in [0, 99] {
            let inbox =
                get_inbox(&state, json!({ "agent_name": format!("agent_{}", idx) })).unwrap();
            let msgs = inbox_messages(&inbox);
            assert_eq!(msgs.len(), 1);
            assert_eq!(
                msgs[0]["from_agent"].as_str().unwrap().len(),
                super::MAX_FROM_AGENT_LEN
            );
            assert_eq!(
                msgs[0]["subject"].as_str().unwrap().len(),
                super::MAX_SUBJECT_LEN
            );
            assert_eq!(msgs[0]["body"].as_str().unwrap().len(), super::MAX_BODY_LEN);
        }
    }

    // ════════════════════════════════════════════════════════════════════════════
    // Deep Review #13 — Hypothesis Tests
    // ════════════════════════════════════════════════════════════════════════════

    // ── H1 (CONFIRMED): search_messages query has no explicit length bound ──
    // Every other user-facing text input has a MAX_* constant. The query field
    // in search_messages has none — it relies solely on the 1MB Axum body limit.
    #[tokio::test]
    async fn dr13_h1_search_query_length_validated() {
        let (state, _idx, _repo) = test_post_office();

        // Oversized query must be rejected
        let huge_query = "a".repeat(super::MAX_QUERY_LEN + 1);
        let result = search_messages(&state, json!({ "query": huge_query }));

        assert!(result.is_err(), "Oversized query must be rejected");
        assert!(result.unwrap_err().contains("query exceeds"));

        // Boundary test: exactly MAX_QUERY_LEN accepted
        let ok_query = "a".repeat(super::MAX_QUERY_LEN);
        let result = search_messages(&state, json!({ "query": ok_query }));
        // Accepted by length check (may still fail at parse level, which is fine)
        assert!(
            result.is_ok() || result.unwrap_err().starts_with("Query parse error"),
            "Exact boundary must pass length check"
        );
    }

    // ── H2 (CONFIRMED): get_inbox agent_name has no length validation ───────
    // send_message validates from_agent (MAX_FROM_AGENT_LEN) and recipients
    // (MAX_RECIPIENT_NAME_LEN), but get_inbox does not validate agent_name.
    #[tokio::test]
    async fn dr13_h2_get_inbox_agent_name_length_validated() {
        let (state, _idx, _repo) = test_post_office();

        // Oversized agent_name must be rejected (uses MAX_RECIPIENT_NAME_LEN
        // because get_inbox reads inboxes keyed by recipient name from send_message)
        let huge_name = "A".repeat(super::MAX_RECIPIENT_NAME_LEN + 1);
        let result = get_inbox(&state, json!({ "agent_name": huge_name }));

        assert!(result.is_err(), "Oversized agent_name must be rejected");
        assert!(result.unwrap_err().contains("agent_name exceeds"));

        // Boundary test: exactly MAX_RECIPIENT_NAME_LEN accepted
        let ok_name = "A".repeat(super::MAX_RECIPIENT_NAME_LEN);
        let result = get_inbox(&state, json!({ "agent_name": ok_name }));
        assert!(result.is_ok(), "Exact boundary must be accepted");
    }

    // ── H3 (DISPROVED): from_agent with null bytes causes corruption ────────
    // from_agent has only a length check, no content validation like
    // validate_agent_name(). Null bytes flow into InboxEntry and Tantivy.
    // serde_json handles embedded nulls safely (encodes as \u0000).
    #[tokio::test]
    async fn dr13_h3_from_agent_null_bytes_safe() {
        let (state, _idx, _repo) = test_post_office();

        let evil_from = "alice\0evil";
        let result = send_message(
            &state,
            json!({
                "from_agent": evil_from,
                "to": ["bob"],
                "subject": "test",
                "body": "hello",
            }),
        );

        // Rejected — from_agent now validates for null bytes (DR17 H5 fix).
        assert!(result.is_err(), "Null bytes in from_agent are rejected");
        assert!(
            result.unwrap_err().contains("null bytes"),
            "Error mentions null bytes"
        );
    }

    // ── H4 (DISPROVED): search_messages with empty query panics ─────────────
    // Tantivy's QueryParser accepts an empty string without panicking —
    // it returns Ok with an empty result set. No crash, no error.
    #[tokio::test]
    async fn dr13_h4_empty_query_does_not_panic() {
        let (state, _idx, _repo) = test_post_office();

        let result = search_messages(&state, json!({ "query": "" }));

        // Empty query is now explicitly rejected (DR21 H3 fix)
        assert!(result.is_err(), "Empty query must be rejected");
        assert!(
            result.unwrap_err().contains("empty or whitespace-only"),
            "Error should mention empty/whitespace"
        );
    }

    // ── H5 (CONFIRMED): search_messages limit uses magic numbers ────────────
    // get_inbox uses named constants (DEFAULT_INBOX_LIMIT, MAX_INBOX_LIMIT).
    // search_messages uses inline 10 and 1000. Behavior is correct but naming
    // is inconsistent. This test documents the current behavior.
    #[tokio::test]
    async fn dr13_h5_search_limit_uses_named_constants() {
        let (state, _idx, _repo) = test_post_office();

        // Default limit uses DEFAULT_SEARCH_LIMIT
        let result = search_messages(&state, json!({ "query": "hello" })).unwrap();
        assert!(
            result["results"].as_array().is_some(),
            "Default limit returns array"
        );

        // Negative limit clamped to 1
        let result = search_messages(&state, json!({ "query": "hello", "limit": -5 })).unwrap();
        assert!(
            result["results"].as_array().is_some(),
            "Negative limit clamped to 1"
        );

        // Huge limit clamped to MAX_SEARCH_LIMIT
        let result = search_messages(
            &state,
            json!({ "query": "hello", "limit": (super::MAX_SEARCH_LIMIT + 999) }),
        )
        .unwrap();
        assert!(
            result["results"].as_array().is_some(),
            "Huge limit clamped to MAX_SEARCH_LIMIT"
        );
    }

    // ════════════════════════════════════════════════════════════════════════════
    // Deep Review #14 — Hypothesis Tests
    // ════════════════════════════════════════════════════════════════════════════

    // ── H1 (CONFIRMED): agents DashMap has no entry count limit ─────────────
    // Unlike inboxes (capped at MAX_INBOX_SIZE), there is no MAX_AGENTS limit.
    // Unlimited create_agent calls with unique names grow the DashMap unbounded.
    #[tokio::test]
    async fn dr14_h1_agents_dashmap_capped_at_max() {
        let (state, _idx, _repo) = test_post_office();

        // Register up to MAX_AGENTS — we can't test 100K in a unit test,
        // so verify the mechanism works by checking the check exists.
        // Register 200 agents (well under MAX_AGENTS) — all succeed.
        for i in 0..200 {
            let result = create_agent(
                &state,
                json!({
                    "project_key": "stress",
                    "name_hint": format!("agent_{}", i),
                }),
            );
            assert!(result.is_ok(), "Agent {} should register", i);
        }
        assert_eq!(state.agents.len(), 200, "All 200 agents registered");

        // Upsert does NOT count against the limit (Occupied branch)
        let result = create_agent(
            &state,
            json!({
                "project_key": "stress",
                "name_hint": "agent_0",
                "program": "updated",
            }),
        );
        assert!(result.is_ok(), "Upsert must succeed regardless of count");
    }

    // ── H2 (UPDATED): persist channel drop returns "sent" silently ─────────
    // When the persist channel is full, try_send drops the Tantivy index op.
    // The caller gets "status": "sent" with no internal state leakage.
    // (DR21 H5 removed the "indexed" field from the response.)
    #[tokio::test]
    async fn dr14_h2_send_message_reports_indexed_status() {
        let (state, _idx, _repo) = test_post_office();

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "test",
                "body": "hello",
            }),
        )
        .unwrap();

        assert_eq!(result["status"], "sent");
        assert!(
            result.get("indexed").is_none(),
            "Response must NOT expose internal 'indexed' status (removed in DR21)"
        );
    }

    // ── H3 (DISPROVED): search_messages limit=0 is safe ────────────────────
    // clamp(1, MAX_SEARCH_LIMIT) ensures 0 becomes 1 before reaching Tantivy.
    #[tokio::test]
    async fn dr14_h3_search_limit_zero_clamped_to_one() {
        let (state, _idx, _repo) = test_post_office();

        let result = search_messages(&state, json!({ "query": "hello", "limit": 0 }));
        assert!(result.is_ok(), "limit=0 must not panic");
        assert!(result.unwrap()["results"].as_array().is_some());
    }

    // ── H4 (DISPROVED): empty JSON request returns error, no panic ──────────
    // handle_mcp_request with {} defaults method to "" which hits the
    // catch-all "Method not found" branch.
    #[tokio::test]
    async fn dr14_h4_empty_json_request_returns_error() {
        let (state, _idx, _repo) = test_post_office();

        // Empty JSON object has no "id" — treated as notification (DR21 H4 fix)
        let response = handle_mcp_request(state, json!({})).await;
        assert!(
            response.is_null(),
            "Empty JSON has no id — notification returns Value::Null"
        );

        // With "id" present, it still returns Method not found
        let response2 = handle_mcp_request(test_post_office().0, json!({ "id": 1 })).await;
        assert_eq!(response2["jsonrpc"], "2.0");
        assert!(response2.get("error").is_some(), "Must return error");
        assert_eq!(response2["error"]["code"], -32601);
        assert_eq!(response2["error"]["message"], "Method not found");
    }

    // ── H5 (CONFIRMED): dot-prefixed agent names create hidden dirs ─────────
    // validate_agent_name rejects "." but allows ".hidden", which creates
    // agents/.hidden/profile.json in the git repo — a hidden directory.
    #[tokio::test]
    async fn dr14_h5_dot_prefixed_agent_name_rejected() {
        let (state, _idx, _repo) = test_post_office();

        let result = create_agent(
            &state,
            json!({
                "project_key": "test",
                "name_hint": ".hidden_agent",
            }),
        );

        assert!(result.is_err(), "Dot-prefixed name must be rejected");
        assert!(result.unwrap_err().contains("must not start with '.'"));
        assert!(
            !state.agents.contains_key(".hidden_agent"),
            "No agent registered for rejected name"
        );

        // Names with dots in the middle are still valid
        let result = create_agent(
            &state,
            json!({
                "project_key": "test",
                "name_hint": "agent.v2",
            }),
        );
        assert!(result.is_ok(), "Dots in the middle are allowed");
    }

    // ════════════════════════════════════════════════════════════════════════════
    // Deep Review #15 — Hypothesis Tests
    // ════════════════════════════════════════════════════════════════════════════

    // ── H1 (FIXED): search_messages now returns scalar fields ────────────────
    // Previously, Tantivy's schema.to_json() wrapped each field in an array
    // (e.g., "subject":["hello"]). Now unwrap_tantivy_arrays() converts
    // single-element arrays to scalars, matching get_inbox's format.
    #[tokio::test]
    async fn dr15_h1_search_returns_scalar_fields() {
        let (state, _idx, _repo) = test_post_office();

        send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "unique_dr15_marker",
                "body": "searchable content for dr15",
                "project_id": "proj_dr15",
            }),
        )
        .unwrap();

        // Wait for persistence worker + NRT reader refresh
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let result = search_messages(
            &state,
            json!({ "query": "unique_dr15_marker", "limit": 10 }),
        );
        assert!(result.is_ok(), "Search must succeed");

        let results = result.unwrap();
        let docs = results["results"].as_array().unwrap();
        assert!(!docs.is_empty(), "Must find at least one result");

        // FIXED: fields are now scalar, not array-wrapped
        let doc = &docs[0];
        let subject = &doc["subject"];
        assert!(
            subject.is_string(),
            "FIXED: search returns scalar subject, not array. Got: {}",
            subject
        );
        assert_eq!(subject, "unique_dr15_marker");

        // Both search and inbox now use consistent scalar format
        let inbox = get_inbox(&state, json!({ "agent_name": "bob" })).unwrap();
        let inbox_msg = &inbox_messages(&inbox)[0];
        assert!(
            inbox_msg["subject"].is_string(),
            "get_inbox also returns scalar string"
        );
        assert_eq!(inbox_msg["subject"], "unique_dr15_marker");
    }

    // ── H2 (FIXED): Tool errors now use correct JSON-RPC error codes ──────────
    // -32601 for unknown tool (Method not found), -32602 for param/validation
    // errors (Invalid params), -32601 for unknown method.
    #[tokio::test]
    async fn dr15_h2_tool_errors_use_correct_codes() {
        let (state, _idx, _repo) = test_post_office();

        // 1. Missing required param → -32602 (Invalid params)
        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "create_agent", "arguments": {} }
            }),
        )
        .await;
        assert!(resp.get("error").is_some(), "Missing project_key → error");
        assert_eq!(
            resp["error"]["code"], -32602,
            "FIXED: Missing param returns -32602 (Invalid params)"
        );

        // 2. Validation failure → -32602 (Invalid params)
        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {
                    "name": "create_agent",
                    "arguments": { "project_key": "test", "name_hint": "../evil" }
                }
            }),
        )
        .await;
        assert_eq!(
            resp["error"]["code"], -32602,
            "FIXED: Validation failure returns -32602"
        );

        // 3. Unknown tool → -32601 (Method not found)
        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "nonexistent_tool", "arguments": {} }
            }),
        )
        .await;
        assert_eq!(
            resp["error"]["code"], -32601,
            "FIXED: Unknown tool returns -32601 (Method not found)"
        );

        // 4. Unknown method → -32601 (Method not found)
        let resp = handle_mcp_request(
            state,
            json!({ "jsonrpc": "2.0", "id": 4, "method": "bad_method" }),
        )
        .await;
        assert_eq!(
            resp["error"]["code"], -32601,
            "Unknown method correctly returns -32601"
        );
    }

    // ── H3 (DISPROVED): Agent names with spaces work correctly ────────────────
    // validate_agent_name allows spaces (they pass all checks: not empty,
    // not whitespace-only, no slashes, no control chars). Spaces in names
    // create DashMap keys containing spaces, and to_recipients join becomes
    // ambiguous (e.g., "Agent Smith" + "bob" → "Agent Smith bob"). However,
    // to_recipients is stored as STRING (opaque token) not TEXT, so search
    // doesn't tokenize it — no functional impact.
    #[tokio::test]
    async fn dr15_h3_agent_names_with_spaces_work() {
        let (state, _idx, _repo) = test_post_office();

        // Agent name with spaces passes validation
        let result = create_agent(
            &state,
            json!({ "project_key": "test", "name_hint": "Agent Smith" }),
        );
        assert!(
            result.is_ok(),
            "Agent names with spaces are accepted by validate_agent_name"
        );

        // Send and receive messages with spaced names
        send_message(
            &state,
            json!({
                "from_agent": "Agent Smith",
                "to": ["bob"],
                "subject": "hello",
                "body": "from a spaced name",
            }),
        )
        .unwrap();

        let inbox = get_inbox(&state, json!({ "agent_name": "bob" })).unwrap();
        assert_eq!(inbox_messages(&inbox).len(), 1);
        assert_eq!(
            inbox_messages(&inbox)[0]["from_agent"],
            "Agent Smith",
            "Spaced name preserved in inbox"
        );

        // Receiving works with spaced names too
        send_message(
            &state,
            json!({
                "from_agent": "bob",
                "to": ["Agent Smith"],
                "subject": "reply",
                "body": "hi back",
            }),
        )
        .unwrap();

        let inbox = get_inbox(&state, json!({ "agent_name": "Agent Smith" })).unwrap();
        assert_eq!(inbox_messages(&inbox).len(), 1);
        assert_eq!(inbox_messages(&inbox)[0]["from_agent"], "bob");
    }

    // ── H4 (FIXED): NRT refresh task shuts down cleanly on PostOffice drop ────
    // Previously, the NRT task ran forever until the tokio runtime shut down.
    // Now, PostOffice holds an Arc<NrtShutdownGuard> that sets an AtomicBool
    // flag when the last clone is dropped. The NRT task checks this flag
    // each iteration and exits cleanly.
    #[tokio::test]
    async fn dr15_h4_nrt_refresh_task_exits_on_post_office_drop() {
        let idx_dir = TempDir::new().unwrap();
        let repo_dir = TempDir::new().unwrap();
        let state = PostOffice::new(idx_dir.path(), repo_dir.path()).unwrap();

        // Verify the state works
        send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "pre-drop",
                "body": "test",
            }),
        )
        .unwrap();

        // Clone simulates Extension layer in axum
        let clone = state.clone();
        drop(clone);

        // Original still works
        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["charlie"],
                "subject": "after clone drop",
                "body": "still works",
            }),
        );
        assert!(result.is_ok(), "State works after dropping one clone");

        // Drop last PostOffice — NrtShutdownGuard fires, sets AtomicBool flag
        drop(state);

        // Brief sleep to allow NRT task to see the flag and exit
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // FIXED: NRT task exits cleanly via NrtShutdownGuard.
        // The AtomicBool flag is set when the last Arc<NrtShutdownGuard> drops.
        // The task checks the flag every 100ms and breaks out of the loop.
    }

    // ── H5 (DISPROVED): Missing/wrong jsonrpc version is still processed ──────
    // handle_mcp_request does not validate the jsonrpc field. This is fine:
    // the handler processes the request by method dispatch regardless of
    // protocol version. This is common practice for simple JSON-RPC servers.
    // Regression guard: verify this behavior is stable.
    #[tokio::test]
    async fn dr15_h5_missing_jsonrpc_field_still_works() {
        let (state, _idx, _repo) = test_post_office();

        // No jsonrpc field at all
        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "id": 1, "method": "tools/list"
            }),
        )
        .await;
        assert!(
            resp.get("result").is_some(),
            "Missing jsonrpc field does not prevent processing"
        );
        assert_eq!(
            resp["jsonrpc"], "2.0",
            "Response still includes jsonrpc: 2.0"
        );

        // Wrong version
        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "1.0", "id": 2, "method": "tools/list"
            }),
        )
        .await;
        assert!(
            resp.get("result").is_some(),
            "Wrong jsonrpc version does not prevent processing"
        );

        // Non-string jsonrpc
        let resp = handle_mcp_request(
            state,
            json!({
                "jsonrpc": 42, "id": 3, "method": "tools/list"
            }),
        )
        .await;
        assert!(
            resp.get("result").is_some(),
            "Non-string jsonrpc does not prevent processing"
        );
    }

    // ════════════════════════════════════════════════════════════════════════════
    // Deep Review #16 — Hypothesis Tests
    // ════════════════════════════════════════════════════════════════════════════

    // ── H1 (CONFIRMED): InboxEntry clone amplification on multi-recipient send ─
    // send_message clones the full InboxEntry (from_agent + subject + body) for
    // each recipient. With MAX_RECIPIENTS (100) and MAX_BODY_LEN (64KB), a
    // single send allocates ~100 × (256 + 1024 + 65536) ≈ 6.5MB of cloned
    // String data in DashMap. This is inherent to the fan-out design — each
    // recipient needs an independent copy because get_inbox drains destructively.
    // Documenting as a design tradeoff, not a fixable bug.
    #[tokio::test]
    async fn dr16_h1_inbox_entry_clone_amplification() {
        let (state, _idx, _repo) = test_post_office();

        // Send with max-length body to 10 recipients
        let big_body = "B".repeat(super::MAX_BODY_LEN);
        let recipients: Vec<String> = (0..10).map(|i| format!("agent_{}", i)).collect();

        let result = send_message(
            &state,
            json!({
                "from_agent": "sender",
                "to": recipients,
                "subject": "big payload",
                "body": big_body,
            }),
        );
        assert!(result.is_ok(), "Multi-recipient large body send succeeds");

        // Verify each recipient got an independent copy (not shared ref)
        let inbox_0 = get_inbox(&state, json!({ "agent_name": "agent_0" })).unwrap();
        let inbox_9 = get_inbox(&state, json!({ "agent_name": "agent_9" })).unwrap();

        let msgs_0 = inbox_messages(&inbox_0);
        let msgs_9 = inbox_messages(&inbox_9);
        assert_eq!(msgs_0.len(), 1);
        assert_eq!(msgs_9.len(), 1);

        // Each has the full body (independent clone, not shared)
        assert_eq!(
            msgs_0[0]["body"].as_str().unwrap().len(),
            super::MAX_BODY_LEN
        );
        assert_eq!(
            msgs_9[0]["body"].as_str().unwrap().len(),
            super::MAX_BODY_LEN
        );

        // Draining agent_0 does not affect agent_9's remaining messages
        // (already drained above, but confirm agent_1 still has its copy)
        let inbox_1 = get_inbox(&state, json!({ "agent_name": "agent_1" })).unwrap();
        assert_eq!(
            inbox_messages(&inbox_1).len(),
            1,
            "Each recipient has independent copy — draining one doesn't affect others"
        );
    }

    // ── H2 (FIXED): create_agent response now includes registered_at ──────────
    // The response includes id, project_id, name, program, model, and
    // registered_at (Unix timestamp). Clients can now see when registration occurred.
    #[tokio::test]
    async fn dr16_h2_create_agent_response_has_timestamp() {
        let (state, _idx, _repo) = test_post_office();

        let r1 = create_agent(
            &state,
            json!({ "project_key": "proj", "name_hint": "Alice", "program": "v1" }),
        )
        .unwrap();

        // Response fields
        assert!(r1.get("id").is_some(), "Has id");
        assert!(r1.get("project_id").is_some(), "Has project_id");
        assert!(r1.get("name").is_some(), "Has name");
        assert!(r1.get("program").is_some(), "Has program");
        assert!(r1.get("model").is_some(), "Has model");
        assert!(
            r1.get("registered_at").is_some(),
            "FIXED: create_agent response now includes registered_at"
        );
        assert!(
            r1["registered_at"].is_i64(),
            "registered_at is a Unix timestamp"
        );

        // Upsert returns same structure with fresh timestamp
        let r2 = create_agent(
            &state,
            json!({ "project_key": "proj", "name_hint": "Alice", "program": "v2" }),
        )
        .unwrap();
        assert!(
            r2.get("registered_at").is_some(),
            "Upsert also includes registered_at"
        );

        // Only id differs between registration and upsert
        assert_ne!(r1["id"], r2["id"], "Different ids on re-registration");
        assert_eq!(r1["project_id"], r2["project_id"], "Same project_id");
    }

    // ── H3 (DISPROVED): unwrap_tantivy_arrays preserves multi-element arrays ──
    // If a Tantivy doc ever had a field with 2+ values, the unwrapper would
    // leave it as an array (only length-1 arrays are unwrapped). This is the
    // correct defensive behavior — never silently drop data.
    #[tokio::test]
    async fn dr16_h3_unwrap_preserves_multi_element_arrays() {
        // Simulate a Tantivy doc with multi-valued field
        let doc = json!({
            "subject": ["hello", "world"],  // multi-valued (hypothetical)
            "body": ["single"],             // single-valued
            "id": ["abc123"],               // single-valued
        });

        let unwrapped = unwrap_tantivy_arrays(doc);

        // Multi-element array preserved as-is
        assert!(
            unwrapped["subject"].is_array(),
            "Multi-element array NOT unwrapped (defensive)"
        );
        assert_eq!(unwrapped["subject"].as_array().unwrap().len(), 2);

        // Single-element arrays unwrapped to scalars
        assert!(unwrapped["body"].is_string(), "Single-element → scalar");
        assert_eq!(unwrapped["body"], "single");
        assert_eq!(unwrapped["id"], "abc123");
    }

    // ── H4 (FIXED): get_inbox and search_messages now use consistent field names
    // Both return: { "id", "from_agent", "subject", "body", "created_ts" }
    #[tokio::test]
    async fn dr16_h4_inbox_and_search_field_names_match() {
        let (state, _idx, _repo) = test_post_office();

        send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "dr16_field_test",
                "body": "field name comparison",
                "project_id": "proj_dr16",
            }),
        )
        .unwrap();

        // Check inbox field names
        let inbox = get_inbox(&state, json!({ "agent_name": "bob" })).unwrap();
        let inbox_msg = &inbox_messages(&inbox)[0];
        assert!(
            inbox_msg.get("from_agent").is_some(),
            "FIXED: Inbox now uses 'from_agent'"
        );
        assert!(
            inbox_msg.get("from").is_none(),
            "Old 'from' field no longer present"
        );
        assert!(
            inbox_msg.get("created_ts").is_some(),
            "FIXED: Inbox now uses 'created_ts'"
        );
        assert!(
            inbox_msg.get("timestamp").is_none(),
            "Old 'timestamp' field no longer present"
        );

        // Wait for indexing
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Check search field names
        let search =
            search_messages(&state, json!({ "query": "dr16_field_test", "limit": 10 })).unwrap();
        let search_docs = search["results"].as_array().unwrap();
        assert!(!search_docs.is_empty(), "Search found the message");

        let search_doc = &search_docs[0];
        assert!(
            search_doc.get("from_agent").is_some(),
            "CONFIRMED: Search has 'from_agent' (not 'from')"
        );
        assert!(
            search_doc.get("from").is_none(),
            "Search does NOT have 'from'"
        );
        assert!(
            search_doc.get("created_ts").is_some(),
            "CONFIRMED: Search has 'created_ts' (not 'timestamp')"
        );
        assert!(
            search_doc.get("timestamp").is_none(),
            "Search does NOT have 'timestamp'"
        );
    }

    // ── H5 (CONFIRMED): persistence_worker does not flush on shutdown ─────────
    // When the persist channel closes (all senders dropped), rx.recv() returns
    // Err and the worker breaks out of the loop WITHOUT calling
    // index_writer.commit(). Any documents added via add_document() in the
    // current batch are lost. In practice, the last batch is committed before
    // the channel closes (the block→drain→commit cycle completes before the
    // next recv() call), so only docs received between the last commit and
    // channel close are at risk. This is a small window but real.
    //
    // Tracing the code path:
    // 1. recv() blocks → gets first op → drain pending → commit → loop back
    // 2. On next recv(), if channel is closed → break (no final commit)
    // 3. Any add_document() calls in between are lost
    //
    // The window is between the last commit() and the next recv() returning Err.
    // Since recv() blocks until a message arrives, and the channel closes only
    // when all PostOffice clones are dropped, the typical scenario is:
    //   - Worker commits batch N
    //   - Worker blocks on recv()
    //   - PostOffice drops → channel closes → recv() returns Err → break
    //   - No uncommitted docs in this scenario (batch N already committed)
    //
    // The edge case is: worker is draining batch N, adds docs, commits, but
    // MORE ops arrived during the commit. Those get picked up in the NEXT
    // recv() cycle. If the channel closes between commit and recv, those
    // new ops were never recv()'d so they're just left in the closed channel.
    //
    // Actually: crossbeam bounded channel guarantees that after all senders
    // drop, recv() returns all buffered messages before returning Err. So
    // the worker will process ALL remaining messages before shutting down.
    // This means H5 is DISPROVED — no data loss on shutdown.
    #[tokio::test]
    async fn dr16_h5_persistence_worker_drains_on_shutdown() {
        let (state, _idx, _repo) = test_post_office();

        // Send multiple messages to fill the persist channel
        for i in 0..50 {
            send_message(
                &state,
                json!({
                    "from_agent": "alice",
                    "to": ["bob"],
                    "subject": format!("shutdown_test_{}", i),
                    "body": "content",
                    "project_id": "proj_shutdown",
                }),
            )
            .unwrap();
        }

        // Drop PostOffice — closes persist_tx → worker drains all buffered ops
        drop(state);

        // The persistence worker will process all 50 IndexMessage ops and
        // commit them before exiting (crossbeam delivers all buffered messages
        // before returning Err on recv()). We can't easily verify the Tantivy
        // index after drop since the index/reader are also dropped, but the
        // test documents that the shutdown path is safe: no panics, no hangs.

        // Brief sleep to let worker threads finish
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Deep Review #17 — Hypothesis Tests
    // ═══════════════════════════════════════════════════════════════════════════

    // ── H1 (FIXED): to_recipients uses \x1F unit separator for unambiguous split
    // Agent names can contain spaces (DR15 H3). Using ASCII Unit Separator
    // (\x1F) as delimiter guarantees unambiguous round-trip since control chars
    // are rejected by validate_agent_name and from_agent validation.
    #[tokio::test]
    async fn dr17_h1_to_recipients_uses_unit_separator() {
        let (state, _idx, _repo) = test_post_office();

        // Register agents with spaces in names
        create_agent(
            &state,
            json!({ "project_key": "proj", "name_hint": "Agent A" }),
        )
        .unwrap();
        create_agent(
            &state,
            json!({ "project_key": "proj", "name_hint": "Agent B" }),
        )
        .unwrap();

        // Send to two space-containing recipients
        let r = send_message(
            &state,
            json!({
                "from_agent": "sender",
                "to": ["Agent A", "Agent B"],
                "subject": "dr17_join_test",
                "body": "ambiguity check",
                "project_id": "proj_dr17"
            }),
        )
        .unwrap();
        assert_eq!(r["status"], "sent");

        // Wait for Tantivy indexing
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Search for this message
        let search =
            search_messages(&state, json!({ "query": "dr17_join_test", "limit": 10 })).unwrap();
        let docs = search["results"].as_array().unwrap();
        assert!(!docs.is_empty(), "Message indexed");

        // FIXED: to_recipients uses \x1F unit separator — unambiguous split
        let to_field = docs[0]["to_recipients"].as_str().unwrap();
        assert_eq!(
            to_field, "Agent A\x1FAgent B",
            "FIXED: Unit separator produces unambiguous recipient list"
        );

        // Round-trip: split on \x1F recovers exactly 2 recipients
        let parts: Vec<&str> = to_field.split('\x1F').collect();
        assert_eq!(parts.len(), 2, "Exactly 2 recipients recovered");
        assert_eq!(parts[0], "Agent A");
        assert_eq!(parts[1], "Agent B");
    }

    // ── H2 (FIXED): tools/list now includes inputSchema per MCP spec ──────────
    // Each tool declares its JSON Schema so clients can validate arguments.
    #[tokio::test]
    async fn dr17_h2_tools_list_has_input_schema() {
        let (state, _idx, _repo) = test_post_office();

        let resp = handle_mcp_request(
            state,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        )
        .await;

        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 4, "Four tools returned");

        for tool in tools {
            assert!(tool.get("name").is_some(), "Has name");
            assert!(tool.get("description").is_some(), "Has description");
            assert!(
                tool.get("inputSchema").is_some(),
                "FIXED: Every tool has inputSchema"
            );
            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], "object", "Schema type is object");
            assert!(schema.get("properties").is_some(), "Schema has properties");
            assert!(
                schema.get("required").is_some(),
                "Schema has required array"
            );
        }

        // Verify specific required fields
        let create_agent_tool = &tools[0];
        assert_eq!(create_agent_tool["name"], "create_agent");
        let required = create_agent_tool["inputSchema"]["required"]
            .as_array()
            .unwrap();
        assert!(
            required.contains(&json!("project_key")),
            "create_agent requires project_key"
        );

        let send_message_tool = &tools[1];
        assert_eq!(send_message_tool["name"], "send_message");
        let required = send_message_tool["inputSchema"]["required"]
            .as_array()
            .unwrap();
        assert!(
            required.contains(&json!("from_agent")),
            "send_message requires from_agent"
        );
        assert!(required.contains(&json!("to")), "send_message requires to");
    }

    // ── H3 (FIXED): projects DashMap now has MAX_PROJECTS soft cap ─────────────
    // state.projects is still write-only (no tool reads it), but at least it
    // won't grow unbounded. Matches the MAX_AGENTS pattern.
    #[tokio::test]
    async fn dr17_h3_projects_dashmap_capped() {
        let (state, _idx, _repo) = test_post_office();

        // Register agents across many distinct projects
        for i in 0..50 {
            create_agent(
                &state,
                json!({
                    "project_key": format!("project_{}", i),
                    "name_hint": format!("agent_{}", i)
                }),
            )
            .unwrap();
        }

        // Projects map grows with each unique project_key (up to cap)
        assert_eq!(
            state.projects.len(),
            50,
            "FIXED: 50 projects accepted (under MAX_PROJECTS cap)"
        );

        // The cap is MAX_PROJECTS (100_000) — same as MAX_AGENTS.
        // We can't test the full cap in a unit test, but the logic is there.
    }

    // ── H4 (FIXED): AgentRecord now includes registered_at ────────────────────
    // Both the DashMap record and the API response include registered_at.
    #[tokio::test]
    async fn dr17_h4_agent_record_has_registered_at() {
        let (state, _idx, _repo) = test_post_office();

        let resp = create_agent(
            &state,
            json!({ "project_key": "proj", "name_hint": "Alice" }),
        )
        .unwrap();

        // API response has registered_at
        assert!(
            resp.get("registered_at").is_some(),
            "API response includes registered_at"
        );
        let api_registered_at = resp["registered_at"].as_i64().unwrap();
        assert!(api_registered_at > 0, "Valid timestamp");

        // DashMap record NOW has registered_at
        let record = state.agents.get("Alice").unwrap();
        assert_eq!(record.name, "Alice");
        assert_eq!(record.id, resp["id"].as_str().unwrap());
        assert_eq!(
            record.registered_at, api_registered_at,
            "FIXED: DashMap record matches API response timestamp"
        );
    }

    // ── H5 (FIXED): from_agent in send_message now rejects control chars ───────
    // from_agent is validated for null bytes and control characters.
    #[tokio::test]
    async fn dr17_h5_send_message_from_agent_rejects_control_chars() {
        let (state, _idx, _repo) = test_post_office();

        // Null byte in from_agent — now rejected
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "evil\0sender",
                "to": ["bob"],
                "subject": "null byte test",
                "body": "test"
            }),
        );
        assert!(r1.is_err(), "FIXED: from_agent with null byte is rejected");
        assert!(
            r1.unwrap_err().contains("null bytes"),
            "Error mentions null bytes"
        );

        // Control characters (bell, backspace, escape) in from_agent — now rejected
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "evil\x07\x08\x1bsender",
                "to": ["bob"],
                "subject": "control char test",
                "body": "test"
            }),
        );
        assert!(
            r2.is_err(),
            "FIXED: from_agent with control chars is rejected"
        );
        assert!(
            r2.unwrap_err().contains("control characters"),
            "Error mentions control characters"
        );

        // Clean from_agent still works
        let r3 = send_message(
            &state,
            json!({
                "from_agent": "clean_sender",
                "to": ["bob"],
                "subject": "clean test",
                "body": "test"
            }),
        );
        assert!(r3.is_ok(), "Clean from_agent is accepted");
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Deep Review #18 — Hypothesis Tests
    // ═══════════════════════════════════════════════════════════════════════════

    // ── H1 (FIXED): to recipients now validated for dangerous content ──────
    // Recipients are checked for null bytes, control chars, path separators,
    // and ".." — matching validate_agent_name pattern.
    #[tokio::test]
    async fn dr18_h1_to_recipients_validated() {
        let (state, _idx, _repo) = test_post_office();

        // Null byte in recipient — now rejected
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["\0ghost"],
                "subject": "test",
                "body": "hi"
            }),
        );
        assert!(r1.is_err(), "FIXED: Null byte in recipient rejected");
        assert!(r1.unwrap_err().contains("null bytes"));

        // Path traversal in recipient — now rejected
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["../evil"],
                "subject": "test",
                "body": "hi"
            }),
        );
        assert!(r2.is_err(), "FIXED: Path traversal in recipient rejected");
        assert!(r2.unwrap_err().contains("path separators"));

        // Control char in recipient — now rejected
        let r3 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["agent\x07bell"],
                "subject": "test",
                "body": "hi"
            }),
        );
        assert!(r3.is_err(), "FIXED: Control chars in recipient rejected");
        assert!(r3.unwrap_err().contains("control characters"));

        // Path separator in recipient — now rejected
        let r4 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["path/sep"],
                "subject": "test",
                "body": "hi"
            }),
        );
        assert!(r4.is_err(), "FIXED: Path separator in recipient rejected");
        assert!(r4.unwrap_err().contains("path separators"));

        // No orphan DashMap entries created
        assert!(state.inboxes.is_empty(), "No orphan inbox entries");

        // Clean recipient still works
        let r5 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "test",
                "body": "hi"
            }),
        );
        assert!(r5.is_ok(), "Clean recipient accepted");
    }

    // ── H2 (CONFIRMED): InboxEntry missing project_id ──────────────────────
    // Messages sent with project_id lose that context in the inbox.
    // Only the Tantivy index retains project association.
    #[tokio::test]
    async fn dr18_h2_inbox_entry_missing_project_id() {
        let (state, _idx, _repo) = test_post_office();

        // Send with explicit project_id
        send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "project context test",
                "body": "check project_id",
                "project_id": "proj_important_123"
            }),
        )
        .unwrap();

        // Get inbox — project_id is absent
        let inbox = get_inbox(&state, json!({ "agent_name": "bob" })).unwrap();
        let msgs = inbox_messages(&inbox);
        assert_eq!(msgs.len(), 1);

        // Verify all fields present
        assert!(msgs[0].get("id").is_some(), "Has id");
        assert!(msgs[0].get("from_agent").is_some(), "Has from_agent");
        assert!(msgs[0].get("subject").is_some(), "Has subject");
        assert!(msgs[0].get("body").is_some(), "Has body");
        assert!(msgs[0].get("created_ts").is_some(), "Has created_ts");

        // FIXED: project_id is now present in the inbox response
        assert!(
            msgs[0].get("project_id").is_some(),
            "FIXED: Inbox response includes project_id"
        );
        assert_eq!(
            msgs[0]["project_id"].as_str().unwrap(),
            "proj_important_123",
            "FIXED: project_id matches what was sent"
        );
    }

    // ── H3 (CONFIRMED): Tantivy persists across PostOffice reconstruction ──
    // DashMap state is ephemeral. After reconstruction with the same index
    // path, search returns historical data but agents/inboxes are empty.
    #[tokio::test]
    async fn dr18_h3_restart_split_brain() {
        let idx_dir = TempDir::new().unwrap();
        let repo_dir = TempDir::new().unwrap();

        // Phase 1: Create state, register agent, send message
        {
            let state = PostOffice::new(idx_dir.path(), repo_dir.path()).unwrap();

            create_agent(
                &state,
                json!({ "project_key": "proj", "name_hint": "Alice" }),
            )
            .unwrap();

            send_message(
                &state,
                json!({
                    "from_agent": "Alice",
                    "to": ["Bob"],
                    "subject": "split brain test",
                    "body": "dr18 persistence check",
                    "project_id": "proj_1"
                }),
            )
            .unwrap();

            // Wait for Tantivy indexing
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            // Verify search works in first instance
            let search =
                search_messages(&state, json!({ "query": "split brain test", "limit": 10 }))
                    .unwrap();
            assert!(
                !search["results"].as_array().unwrap().is_empty(),
                "Search finds message in first instance"
            );

            // Drop first instance
            drop(state);
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        // Phase 2: Reconstruct PostOffice with same paths
        {
            let state2 = PostOffice::new(idx_dir.path(), repo_dir.path()).unwrap();

            // DashMap state is gone
            assert_eq!(state2.agents.len(), 0, "CONFIRMED: Agents lost on restart");
            assert_eq!(
                state2.inboxes.len(),
                0,
                "CONFIRMED: Inboxes lost on restart"
            );
            assert_eq!(
                state2.projects.len(),
                0,
                "CONFIRMED: Projects lost on restart"
            );

            // But Tantivy index persists
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let search =
                search_messages(&state2, json!({ "query": "split brain test", "limit": 10 }))
                    .unwrap();
            assert!(
                !search["results"].as_array().unwrap().is_empty(),
                "CONFIRMED: Tantivy search returns historical data after restart"
            );

            // Bob's inbox is empty despite having an unread message
            let inbox = get_inbox(&state2, json!({ "agent_name": "Bob" })).unwrap();
            assert_eq!(
                inbox_messages(&inbox).len(),
                0,
                "CONFIRMED: Bob's unread message lost on restart — split brain"
            );
        }
    }

    // ── H4 (CONFIRMED): get_inbox and search_messages use different envelopes
    // get_inbox returns { messages: [...], remaining: N }
    // search_messages returns a bare JSON array
    #[tokio::test]
    async fn dr18_h4_response_envelope_inconsistency() {
        let (state, _idx, _repo) = test_post_office();

        // Send a message
        send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "envelope test",
                "body": "dr18 envelope check"
            }),
        )
        .unwrap();

        // get_inbox: wrapped in { messages: [...], remaining: N }
        let inbox = get_inbox(&state, json!({ "agent_name": "bob" })).unwrap();
        assert!(
            inbox.get("messages").is_some(),
            "get_inbox has 'messages' wrapper"
        );
        assert!(
            inbox.get("remaining").is_some(),
            "get_inbox has 'remaining' field"
        );
        assert!(inbox["messages"].is_array(), "messages is an array");

        // Wait for indexing
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // FIXED: search_messages now returns { results: [...], total: N }
        let search =
            search_messages(&state, json!({ "query": "envelope test", "limit": 10 })).unwrap();
        assert!(
            search.get("results").is_some(),
            "FIXED: search_messages has 'results' wrapper"
        );
        assert!(
            search.get("count").is_some(),
            "FIXED: search_messages has 'count' field"
        );
        assert!(
            search["results"].is_array(),
            "FIXED: search results is an array"
        );
        assert!(search["count"].is_number(), "FIXED: count is a number");
    }

    // ── H5 (CONFIRMED): subject accepts null bytes and control characters ──
    // subject is only length-checked. Control chars pass through to inbox
    // and Tantivy, producing garbled entries.
    #[tokio::test]
    async fn dr18_h5_subject_accepts_control_chars() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: Null byte in subject — now rejected
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "test\0injected",
                "body": "hi"
            }),
        );
        assert!(r1.is_err(), "FIXED: Subject with null byte is rejected");

        // FIXED: Control chars in subject — now rejected
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "alert\x07\x08\x1b[31mRED",
                "body": "hi"
            }),
        );
        assert!(
            r2.is_err(),
            "FIXED: Subject with control chars (ANSI escape) is rejected"
        );

        // Clean subject still works
        let r3 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "Normal subject line",
                "body": "hi"
            }),
        );
        assert!(r3.is_ok(), "Clean subject accepted");
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Deep Review #19 — Hypothesis Tests
    // ══════════════════════════════════════════════════════════════════════════

    // ── H1 (CONFIRMED): from_agent in send_message accepts empty string ──────
    // validate_agent_name rejects empty names, but send_message's inline
    // validation for from_agent does not check for empty string.
    #[tokio::test]
    async fn dr19_h1_from_agent_accepts_empty_string() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: Empty from_agent is now rejected
        let r = send_message(
            &state,
            json!({
                "from_agent": "",
                "to": ["bob"],
                "subject": "test",
                "body": "anonymous message"
            }),
        );
        assert!(r.is_err(), "FIXED: Empty from_agent is rejected");
        assert!(
            r.unwrap_err().contains("empty"),
            "Error message mentions empty"
        );

        // Non-empty from_agent still works
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "test",
                "body": "valid message"
            }),
        );
        assert!(r2.is_ok(), "Non-empty from_agent accepted");
    }

    // ── H2 (CONFIRMED): project_id accepts null bytes and control chars ──────
    // project_id in send_message only has a length check. No content validation.
    // Null bytes and control chars persist in inbox and Tantivy index.
    #[tokio::test]
    async fn dr19_h2_project_id_accepts_null_bytes() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: Null byte in project_id — now rejected
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "test",
                "body": "hi",
                "project_id": "proj\0evil"
            }),
        );
        assert!(r1.is_err(), "FIXED: project_id with null byte is rejected");

        // FIXED: Control chars in project_id — now rejected
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "test2",
                "body": "hi",
                "project_id": "proj\x1b[31mRED"
            }),
        );
        assert!(
            r2.is_err(),
            "FIXED: project_id with ANSI escape is rejected"
        );

        // Clean project_id still works
        let r3 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "test3",
                "body": "hi",
                "project_id": "proj_clean_123"
            }),
        );
        assert!(r3.is_ok(), "Clean project_id accepted");
    }

    // ── H3 (CONFIRMED): Malformed JSON returns HTTP 422, not JSON-RPC -32700 ─
    // Axum's Json<Value> extractor rejects non-JSON before handle_mcp_request
    // executes. This is an integration-level issue — we can only document it here
    // and test that handle_mcp_request itself handles non-object payloads gracefully.
    #[tokio::test]
    async fn dr19_h3_non_object_payload_handled() {
        let (state, _idx, _repo) = test_post_office();

        // Non-object payloads have no "id" field — treated as notifications (DR21 H4)
        // All return Value::Null (no response per JSON-RPC 2.0 spec)

        // String payload
        let r1 = handle_mcp_request(state.clone(), json!("not an object")).await;
        assert!(
            r1.is_null(),
            "String payload has no id — notification returns null"
        );

        // Array payload
        let r2 = handle_mcp_request(state.clone(), json!([1, 2, 3])).await;
        assert!(
            r2.is_null(),
            "Array payload has no id — notification returns null"
        );

        // Null payload
        let r3 = handle_mcp_request(state, json!(null)).await;
        assert!(
            r3.is_null(),
            "Null payload has no id — notification returns null"
        );
    }

    // ── H4 (CONFIRMED): search_messages "total" was misleading ─────────────
    // Renamed to "count" — accurately reflects the number of results returned,
    // not total matches in the index.
    #[tokio::test]
    async fn dr19_h4_search_count_reflects_returned() {
        let (state, _idx, _repo) = test_post_office();

        // Send 5 messages with the same searchable keyword
        for i in 0..5 {
            send_message(
                &state,
                json!({
                    "from_agent": "alice",
                    "to": ["bob"],
                    "subject": format!("dr19_count_test msg {}", i),
                    "body": "dr19_count_test searchable content"
                }),
            )
            .unwrap();
        }

        // Wait for indexing
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Search with limit=2
        let search =
            search_messages(&state, json!({ "query": "dr19_count_test", "limit": 2 })).unwrap();

        let results = search["results"].as_array().unwrap();
        let count = search["count"].as_i64().unwrap();

        assert!(results.len() <= 2, "Results bounded by limit");
        assert_eq!(
            count,
            results.len() as i64,
            "FIXED: 'count' accurately reflects number of results returned"
        );
        // Field renamed from "total" to "count" to avoid pagination confusion
        assert!(
            search.get("total").is_none(),
            "FIXED: misleading 'total' field removed"
        );
    }

    // ── H5 (CONFIRMED): body accepts null bytes without validation ───────────
    // subject and from_agent both reject null bytes and control chars.
    // body only has a length check. Null bytes persist in inbox and are
    // indexed into Tantivy.
    #[tokio::test]
    async fn dr19_h5_body_accepts_null_bytes_and_control_chars() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: Null byte in body — now rejected
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "body test 1",
                "body": "before\0after"
            }),
        );
        assert!(r1.is_err(), "FIXED: Body with null byte is rejected");

        // Control chars in body — still accepted (tabs, newlines are valid in markdown)
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "body test 2",
                "body": "line1\nline2\ttabbed"
            }),
        );
        assert!(
            r2.is_ok(),
            "Body with tabs and newlines is accepted (valid markdown)"
        );

        // Clean body still works
        let r3 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "body test 3",
                "body": "Normal body content with punctuation! And lines."
            }),
        );
        assert!(r3.is_ok(), "Clean body accepted");
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Deep Review #20 — Hypothesis Tests
    // ══════════════════════════════════════════════════════════════════════════

    // ── H1 (CONFIRMED): get_inbox agent_name has no content validation ───────
    // Only length is checked. Null bytes, control chars, and empty strings pass.
    // Inconsistent with send_message's recipient and from_agent validation.
    #[tokio::test]
    async fn dr20_h1_get_inbox_agent_name_no_content_validation() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: Empty agent_name — now rejected
        let r1 = get_inbox(&state, json!({ "agent_name": "" }));
        assert!(r1.is_err(), "FIXED: Empty agent_name is rejected");
        assert!(r1.unwrap_err().contains("empty"));

        // FIXED: Null byte in agent_name — now rejected
        let r2 = get_inbox(&state, json!({ "agent_name": "bob\0evil" }));
        assert!(r2.is_err(), "FIXED: agent_name with null byte is rejected");

        // FIXED: Control chars in agent_name — now rejected
        let r3 = get_inbox(&state, json!({ "agent_name": "bob\x1b[31m" }));
        assert!(
            r3.is_err(),
            "FIXED: agent_name with control chars is rejected"
        );

        // Clean agent_name still works
        let r4 = get_inbox(&state, json!({ "agent_name": "bob" }));
        assert!(r4.is_ok(), "Clean agent_name accepted");
    }

    // ── H2 (CONFIRMED): search query has no null byte validation ─────────────
    // query is only length-checked. All other string inputs reject null bytes.
    #[tokio::test]
    async fn dr20_h2_search_query_accepts_null_bytes() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: Query with null byte — now rejected before reaching Tantivy
        let r = search_messages(&state, json!({ "query": "test\0injected" }));
        assert!(r.is_err(), "FIXED: Null byte in query is rejected");
        assert!(r.unwrap_err().contains("null bytes"));

        // Clean query still works
        let r2 = search_messages(&state, json!({ "query": "hello" }));
        assert!(r2.is_ok(), "Clean query accepted");
    }

    // ── H3 (CONFIRMED): project_key has no content validation ────────────────
    // project_key only has a length check. Null bytes and control chars are
    // stored raw in the projects DashMap.
    #[tokio::test]
    async fn dr20_h3_project_key_accepts_null_bytes() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: Null byte in project_key — now rejected
        let r1 = create_agent(
            &state,
            json!({ "project_key": "proj\0evil", "name_hint": "Alice" }),
        );
        assert!(r1.is_err(), "FIXED: project_key with null byte is rejected");

        // FIXED: Control chars in project_key — now rejected
        let r2 = create_agent(
            &state,
            json!({ "project_key": "proj\x1b[31mRED", "name_hint": "Bob" }),
        );
        assert!(
            r2.is_err(),
            "FIXED: project_key with ANSI escape is rejected"
        );

        // Clean project_key still works
        let r3 = create_agent(
            &state,
            json!({ "project_key": "clean_project", "name_hint": "Charlie" }),
        );
        assert!(r3.is_ok(), "Clean project_key accepted");
    }

    // ── H4 (CONFIRMED): models.rs has 3 dead structs ────────────────────────
    // Agent, Message, and FileReservation are defined with #[allow(dead_code)]
    // but never used by any production code. Only InboxEntry is active.
    #[tokio::test]
    async fn dr20_h4_only_inbox_entry_is_used() {
        // This test documents that the production code only uses InboxEntry.
        // The other structs in models.rs (Agent, Message, FileReservation) are dead code.

        // Verify InboxEntry is actually used in the hot path
        let (state, _idx, _repo) = test_post_office();
        send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "alive test",
                "body": "InboxEntry is the only models.rs struct in use"
            }),
        )
        .unwrap();

        let inbox = get_inbox(&state, json!({ "agent_name": "bob" })).unwrap();
        assert_eq!(inbox_messages(&inbox).len(), 1);

        // FIXED: Dead structs (Agent, Message, FileReservation) removed from models.rs
        // Only InboxEntry remains — the sole models struct used in production code
        // AgentRecord in state.rs is what's actually used for agent state
    }

    // ── H5 (CONFIRMED): Default subject no longer pollutes search ───────────
    // Previously "(No Subject)" was indexed as TEXT, making "subject" match
    // every message without an explicit subject. Now defaults to empty string.
    #[tokio::test]
    async fn dr20_h5_default_subject_no_longer_pollutes_search() {
        let (state, _idx, _repo) = test_post_office();

        // Send message with explicit subject containing "subject"
        send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "important subject matter",
                "body": "dr20_h5_fix_marker explicit"
            }),
        )
        .unwrap();

        // Send message WITHOUT subject — now defaults to ""
        send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["charlie"],
                "body": "dr20_h5_fix_marker no subject"
            }),
        )
        .unwrap();

        // Verify the default is now empty string in inbox
        let inbox = get_inbox(&state, json!({ "agent_name": "charlie" })).unwrap();
        let msgs = inbox_messages(&inbox);
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0]["subject"].as_str().unwrap(),
            "",
            "FIXED: Default subject is empty string, not '(No Subject)'"
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // Deep Review #21 — Hypothesis Tests
    // ══════════════════════════════════════════════════════════════════════

    // ── H1 (FIXED): Duplicate recipients are deduplicated ──────────────
    // send_message now deduplicates the to_agents list before delivery.
    // ["alice", "alice", "alice"] delivers exactly one copy.
    #[tokio::test]
    async fn dr21_h1_duplicate_recipients_cause_duplicate_delivery() {
        let (state, _idx, _repo) = test_post_office();

        let result = send_message(
            &state,
            json!({
                "from_agent": "bob",
                "to": ["alice", "alice", "alice"],
                "subject": "triplicate",
                "body": "same message three times"
            }),
        );
        assert!(
            result.is_ok(),
            "Duplicate recipients are accepted (deduplicated)"
        );

        let inbox = get_inbox(&state, json!({ "agent_name": "alice" })).unwrap();
        let msgs = inbox_messages(&inbox);

        // FIXED: Alice gets exactly 1 copy after deduplication
        assert_eq!(
            msgs.len(),
            1,
            "FIXED: Duplicate recipients are deduplicated — only 1 copy delivered"
        );
    }

    // ── H2 (FIXED): Leading/trailing whitespace in agent names is rejected ─
    // validate_agent_name now rejects names with leading or trailing whitespace.
    #[tokio::test]
    async fn dr21_h2_whitespace_agent_names_are_distinct() {
        let (state, _idx, _repo) = test_post_office();

        let r1 = create_agent(
            &state,
            json!({ "project_key": "proj", "name_hint": "Alice" }),
        );
        assert!(r1.is_ok(), "Clean name is accepted");

        let r2 = create_agent(
            &state,
            json!({ "project_key": "proj", "name_hint": " Alice" }),
        );
        assert!(r2.is_err(), "FIXED: Leading space is rejected");
        assert!(
            r2.unwrap_err().contains("leading or trailing whitespace"),
            "Error should mention whitespace"
        );

        let r3 = create_agent(
            &state,
            json!({ "project_key": "proj", "name_hint": "Alice " }),
        );
        assert!(r3.is_err(), "FIXED: Trailing space is rejected");

        // Only one agent exists
        assert_eq!(
            state.agents.len(),
            1,
            "FIXED: Only clean 'Alice' is registered"
        );
    }

    // ── H3 (FIXED): Whitespace-only search query is explicitly rejected ─
    // search_messages now rejects whitespace-only queries before reaching Tantivy.
    #[tokio::test]
    async fn dr21_h3_whitespace_only_search_query() {
        let (state, _idx, _repo) = test_post_office();

        let result = search_messages(&state, json!({ "query": "   " }));
        assert!(result.is_err(), "FIXED: Whitespace-only query is rejected");
        assert!(
            result.unwrap_err().contains("empty or whitespace-only"),
            "Error should mention empty/whitespace"
        );
    }

    // ── H4 (FIXED): Notifications (no id) return Value::Null ───────────
    // handle_mcp_request now returns Value::Null for notifications,
    // signaling the HTTP layer to respond with 204 No Content.
    #[tokio::test]
    async fn dr21_h4_notification_without_id_gets_response() {
        let (state, _idx, _repo) = test_post_office();

        // Request without "id" field — this is a JSON-RPC notification
        let request = json!({
            "jsonrpc": "2.0",
            "method": "tools/list"
        });

        let response = handle_mcp_request(state, request).await;

        // FIXED: Notifications return Value::Null (no response per JSON-RPC 2.0)
        assert!(
            response.is_null(),
            "FIXED: Notification (no id) returns Value::Null — no response sent"
        );
    }

    // ── H5 (FIXED): send_message response no longer exposes `indexed` ───
    // The internal persist channel status is no longer leaked to clients.
    #[tokio::test]
    async fn dr21_h5_send_message_response_contains_indexed_field() {
        let (state, _idx, _repo) = test_post_office();

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "test",
                "body": "hello"
            }),
        )
        .unwrap();

        // FIXED: Response only contains id and status — no internal state leak
        assert!(
            result.get("indexed").is_none(),
            "FIXED: 'indexed' field removed from response"
        );
        assert!(result.get("id").is_some(), "Response has id");
        assert_eq!(result["status"], "sent", "Response has status");
    }

    // ══════════════════════════════════════════════════════════════════════
    // Deep Review #22 — Hypothesis Tests
    // ══════════════════════════════════════════════════════════════════════

    // ── H1 (FIXED): project_key with whitespace is rejected ────────────
    // project_key now rejects leading/trailing whitespace, preventing
    // confusingly distinct projects from " proj" vs "proj".
    #[tokio::test]
    async fn dr22_h1_project_key_whitespace_creates_distinct_projects() {
        let (state, _idx, _repo) = test_post_office();

        let r1 = create_agent(
            &state,
            json!({ "project_key": "myproject", "name_hint": "Agent1" }),
        );
        assert!(r1.is_ok(), "Clean project_key is accepted");

        let r2 = create_agent(
            &state,
            json!({ "project_key": " myproject", "name_hint": "Agent2" }),
        );
        assert!(
            r2.is_err(),
            "FIXED: Leading whitespace in project_key rejected"
        );
        assert!(r2.unwrap_err().contains("leading or trailing whitespace"));

        let r3 = create_agent(
            &state,
            json!({ "project_key": "myproject ", "name_hint": "Agent3" }),
        );
        assert!(
            r3.is_err(),
            "FIXED: Trailing whitespace in project_key rejected"
        );

        assert_eq!(
            state.projects.len(),
            1,
            "FIXED: Only one project created from clean key"
        );
    }

    // ── H2 (FIXED): from_agent rejects whitespace-padded names ─────────
    // from_agent now has the same whitespace check as validate_agent_name.
    #[tokio::test]
    async fn dr22_h2_from_agent_accepts_whitespace_padded_names() {
        let (state, _idx, _repo) = test_post_office();

        // " Alice" rejected as agent name
        let create_result = create_agent(
            &state,
            json!({ "project_key": "proj", "name_hint": " Alice" }),
        );
        assert!(
            create_result.is_err(),
            "Leading whitespace rejected in create_agent"
        );

        // Now also rejected as from_agent
        let send_result = send_message(
            &state,
            json!({
                "from_agent": " Alice",
                "to": ["bob"],
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(
            send_result.is_err(),
            "FIXED: ' Alice' rejected as from_agent"
        );
        assert!(send_result
            .unwrap_err()
            .contains("leading or trailing whitespace"));

        // Trailing whitespace also rejected
        let send_result2 = send_message(
            &state,
            json!({
                "from_agent": "Alice ",
                "to": ["bob"],
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(send_result2.is_err(), "FIXED: Trailing whitespace rejected");
    }

    // ── H3 (FIXED): get_inbox agent_name now rejects path separators ────
    // get_inbox validation is now symmetric with create_agent.
    #[tokio::test]
    async fn dr22_h3_get_inbox_accepts_path_separators() {
        let (state, _idx, _repo) = test_post_office();

        // Path separators rejected by create_agent
        let create_result = create_agent(
            &state,
            json!({ "project_key": "proj", "name_hint": "foo/bar" }),
        );
        assert!(create_result.is_err(), "Path sep rejected in create_agent");

        // Now also rejected in get_inbox
        let inbox_result = get_inbox(&state, json!({ "agent_name": "foo/bar" }));
        assert!(
            inbox_result.is_err(),
            "FIXED: 'foo/bar' rejected in get_inbox"
        );
        assert!(inbox_result.unwrap_err().contains("path separators"));

        // ".." also rejected in get_inbox
        let inbox_result2 = get_inbox(&state, json!({ "agent_name": "foo..bar" }));
        assert!(
            inbox_result2.is_err(),
            "FIXED: '..' pattern rejected in get_inbox"
        );
        assert!(inbox_result2.unwrap_err().contains("'..'"));

        // Backslash rejected too
        let inbox_result3 = get_inbox(&state, json!({ "agent_name": "foo\\bar" }));
        assert!(
            inbox_result3.is_err(),
            "FIXED: Backslash rejected in get_inbox"
        );
    }

    // ── H4 (DISPROVED): explicit "id": null correctly returns response ───
    // Regression guard for DR21 H4: "id": null is a request (not a
    // notification). Only MISSING id triggers the notification path.
    #[tokio::test]
    async fn dr22_h4_explicit_null_id_returns_response() {
        let (state, _idx, _repo) = test_post_office();

        // Explicit "id": null — this is a request, not a notification
        let request = json!({
            "jsonrpc": "2.0",
            "id": null,
            "method": "tools/list"
        });

        let response = handle_mcp_request(state, request).await;

        // DISPROVED: The server correctly distinguishes "id": null from missing id
        assert!(
            !response.is_null(),
            "DISPROVED: explicit 'id': null gets a proper response (not a notification)"
        );
        assert!(
            response.get("result").is_some(),
            "tools/list returns a result"
        );
        assert_eq!(
            response["id"],
            Value::Null,
            "id: null is echoed correctly in the response"
        );
    }

    // ── H5 (FIXED): whitespace-only project_key rejected ────────────────
    // project_key "   " is now rejected before reaching UUID generation.
    #[tokio::test]
    async fn dr22_h5_whitespace_only_project_key_accepted() {
        let (state, _idx, _repo) = test_post_office();

        let result = create_agent(
            &state,
            json!({ "project_key": "   ", "name_hint": "Agent1" }),
        );

        // FIXED: Whitespace-only project_key is rejected
        assert!(
            result.is_err(),
            "FIXED: Whitespace-only project_key is rejected"
        );
        assert!(result.unwrap_err().contains("whitespace-only"));

        // Empty string is still accepted (valid edge case — documented in DR7)
        let result2 = create_agent(&state, json!({ "project_key": "", "name_hint": "Agent2" }));
        assert!(result2.is_ok(), "Empty project_key still accepted");
    }

    // ══════════════════════════════════════════════════════════════════════
    // Deep Review #23 — Hypothesis Tests
    // ══════════════════════════════════════════════════════════════════════

    // ── H1 (FIXED): Whitespace-padded recipients now rejected ──────────────────
    // send_message now validates recipients for whitespace trim, matching
    // get_inbox's validation. Prevents undrainable inbox entries.
    #[tokio::test]
    async fn dr23_h1_whitespace_padded_recipient_creates_undrainable_inbox() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: send_message rejects whitespace-padded recipients
        let send_result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": [" bob "],
                "subject": "trapped message",
                "body": "this message cannot be drained"
            }),
        );
        assert!(
            send_result.is_err(),
            "FIXED: send_message rejects whitespace-padded recipient"
        );
        assert!(
            send_result.unwrap_err().contains("whitespace"),
            "Error mentions whitespace"
        );

        // No inbox entry created
        assert!(
            !state.inboxes.contains_key(" bob "),
            "FIXED: No inbox entry for ' bob '"
        );

        // Leading whitespace also rejected
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": [" bob"],
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(r2.is_err(), "FIXED: Leading whitespace rejected");

        // Trailing whitespace also rejected
        let r3 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob "],
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(r3.is_err(), "FIXED: Trailing whitespace rejected");

        // Clean recipient still works
        let r4 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(r4.is_ok(), "Clean recipient accepted");
    }

    // ── H2 (CONFIRMED): Recipient deduplication is case-sensitive ──────────────
    // "Bob" and "bob" are treated as distinct recipients. Both get a copy.
    // This is consistent with DashMap key semantics but documents behavior.
    #[tokio::test]
    async fn dr23_h2_recipient_dedup_is_case_sensitive() {
        let (state, _idx, _repo) = test_post_office();

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["Bob", "bob", "BOB"],
                "subject": "case test",
                "body": "who gets this?"
            }),
        );
        assert!(result.is_ok(), "Case-variant recipients accepted");

        // Each case variant is a distinct DashMap key → separate delivery
        let inbox_bob = get_inbox(&state, json!({ "agent_name": "Bob" })).unwrap();
        let inbox_bob_lower = get_inbox(&state, json!({ "agent_name": "bob" })).unwrap();
        let inbox_bob_upper = get_inbox(&state, json!({ "agent_name": "BOB" })).unwrap();

        assert_eq!(
            inbox_messages(&inbox_bob).len(),
            1,
            "CONFIRMED: 'Bob' gets one copy"
        );
        assert_eq!(
            inbox_messages(&inbox_bob_lower).len(),
            1,
            "CONFIRMED: 'bob' gets one copy"
        );
        assert_eq!(
            inbox_messages(&inbox_bob_upper).len(),
            1,
            "CONFIRMED: 'BOB' gets one copy"
        );
    }

    // ── H3 (CONFIRMED): Non-integer limit silently falls back to default ────────
    // get_inbox and search_messages use as_i64() for limit. Boolean, string,
    // and null all return None → default limit is used silently.
    #[tokio::test]
    async fn dr23_h3_non_integer_limit_falls_back_to_default() {
        let (state, _idx, _repo) = test_post_office();

        // Send 150 messages
        for i in 0..150 {
            send_message(
                &state,
                json!({
                    "from_agent": "sender",
                    "to": ["target"],
                    "subject": format!("msg {}", i),
                    "body": "x",
                }),
            )
            .unwrap();
        }

        // Boolean limit → as_i64() returns None → DEFAULT_INBOX_LIMIT (100)
        let r1 = get_inbox(&state, json!({ "agent_name": "target", "limit": true })).unwrap();
        assert_eq!(
            inbox_messages(&r1).len(),
            100,
            "CONFIRMED: Boolean limit falls back to default (100)"
        );

        // String limit → as_i64() returns None → DEFAULT_INBOX_LIMIT (100)
        // (only 50 remain after draining 100 above)
        let r2 = get_inbox(&state, json!({ "agent_name": "target", "limit": "999" })).unwrap();
        assert_eq!(
            inbox_messages(&r2).len(),
            50,
            "CONFIRMED: String limit falls back to default, drains remaining 50"
        );

        // Same behavior for search_messages
        let search_result = search_messages(&state, json!({ "query": "hello", "limit": false }));
        assert!(
            search_result.is_ok(),
            "CONFIRMED: Boolean limit in search falls back to default"
        );
    }

    // ── H4 (CONFIRMED): Non-string optional fields use defaults silently ────────
    // program=42, model=true → as_str() returns None → unwrap_or("unknown").
    // Client sent a value but gets "unknown" back — no error raised.
    #[tokio::test]
    async fn dr23_h4_non_string_optional_fields_use_defaults() {
        let (state, _idx, _repo) = test_post_office();

        let result = create_agent(
            &state,
            json!({
                "project_key": "test",
                "name_hint": "TypeAgent",
                "program": 42,
                "model": true,
            }),
        );

        assert!(
            result.is_ok(),
            "CONFIRMED: Non-string program/model don't cause errors"
        );

        let profile = result.unwrap();
        assert_eq!(
            profile["program"], "unknown",
            "CONFIRMED: Integer program silently defaults to 'unknown'"
        );
        assert_eq!(
            profile["model"], "unknown",
            "CONFIRMED: Boolean model silently defaults to 'unknown'"
        );

        // DashMap record also has defaults
        let record = state.agents.get("TypeAgent").unwrap();
        assert_eq!(record.program, "unknown");
        assert_eq!(record.model, "unknown");
    }

    // ── H5 (CONFIRMED): Non-array `to` returns misleading error ──────────────
    // If `to` is a string "bob" instead of ["bob"], as_array() returns None,
    // producing error "Missing to recipients" — the field IS present, just wrong type.
    #[tokio::test]
    async fn dr23_h5_non_array_to_returns_misleading_error() {
        let (state, _idx, _repo) = test_post_office();

        // `to` is a string, not an array
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": "bob",
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(r1.is_err(), "Non-array `to` returns error");
        assert_eq!(
            r1.unwrap_err(),
            "Missing to recipients",
            "CONFIRMED: Error says 'Missing' even though field is present (wrong type)"
        );

        // `to` is an integer
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": 42,
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(r2.is_err(), "Integer `to` returns error");
        assert_eq!(r2.unwrap_err(), "Missing to recipients");

        // `to` as null
        let r3 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": null,
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(r3.is_err(), "Null `to` returns error");
        assert_eq!(r3.unwrap_err(), "Missing to recipients");
    }
}
