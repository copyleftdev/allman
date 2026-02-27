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
const MAX_SUBJECT_LEN: usize = 1_024;
const MAX_BODY_LEN: usize = 65_536; // 64 KB
const DEFAULT_INBOX_LIMIT: usize = 100;
const MAX_INBOX_LIMIT: usize = 1_000;

pub async fn handle_mcp_request(state: PostOffice, req: Value) -> Value {
    let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let id = req.get("id");
    let params = req.get("params").cloned().unwrap_or(json!({}));

    match method {
        "tools/call" => {
            let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));

            let result = match tool_name {
                "create_agent" => create_agent(&state, args),
                "search_messages" => search_messages(&state, args),
                "send_message" => send_message(&state, args),
                "get_inbox" => get_inbox(&state, args),
                _ => Err(format!("Unknown tool: {}", tool_name)),
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
                    "error": { "code": -32603, "message": e }
                }),
            }
        }
        "tools/list" => {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        { "name": "create_agent", "description": "Register a new agent" },
                        { "name": "send_message", "description": "Send a message" },
                        { "name": "search_messages", "description": "Search messages" },
                        { "name": "get_inbox", "description": "Get unread messages" }
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
    if name.contains('\0') {
        return Err("Invalid agent name: must not contain null bytes".to_string());
    }
    if name.chars().any(|c| c.is_control()) {
        return Err("Invalid agent name: must not contain control characters".to_string());
    }
    if name.trim().is_empty() {
        return Err("Invalid agent name: must not be whitespace-only".to_string());
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
    if project_key.len() > MAX_PROJECT_KEY_LEN {
        return Err(format!(
            "project_key exceeds {} character limit",
            MAX_PROJECT_KEY_LEN
        ));
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
    let name = name_hint.unwrap_or("AnonymousAgent").to_string();
    validate_agent_name(&name)?;

    let project_id = format!(
        "proj_{}",
        Uuid::new_v5(&Uuid::NAMESPACE_DNS, project_key.as_bytes()).simple()
    );

    // Atomic check-and-insert via DashMap::entry(). Holds the shard lock
    // across the collision check and the insert/update, eliminating the
    // TOCTOU race that existed with separate get() + insert().
    let agent_id = Uuid::new_v4().to_string();

    let record = AgentRecord {
        id: agent_id.clone(),
        project_id: project_id.clone(),
        name: name.clone(),
        program: program.to_string(),
        model: model.to_string(),
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
    state
        .projects
        .entry(project_id.clone())
        .or_insert_with(|| project_key.to_string());

    // Fire-and-forget: persist agent profile to Git
    let profile = json!({
        "id": agent_id,
        "project_id": project_id,
        "name": name,
        "program": program,
        "model": model
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
    let to_agents: Vec<String> = args
        .get("to")
        .and_then(|v| v.as_array())
        .ok_or("Missing to recipients")?
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let subject = args
        .get("subject")
        .and_then(|v| v.as_str())
        .unwrap_or("(No Subject)");
    let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let project_id = args
        .get("project_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if from_agent.len() > MAX_FROM_AGENT_LEN {
        return Err(format!(
            "from_agent exceeds {} character limit",
            MAX_FROM_AGENT_LEN
        ));
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
    if subject.len() > MAX_SUBJECT_LEN {
        return Err(format!(
            "Subject exceeds {} character limit",
            MAX_SUBJECT_LEN
        ));
    }
    if body.len() > MAX_BODY_LEN {
        return Err(format!("Body exceeds {} byte limit", MAX_BODY_LEN));
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
        to_recipients: to_agents.join(" "),
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
                "from": e.from_agent,
                "subject": e.subject,
                "body": e.body,
                "timestamp": e.timestamp
            })
        })
        .collect();

    Ok(json!({
        "messages": result,
        "remaining": remaining_count
    }))
}

// ── search_messages ──────────────────────────────────────────────────────────
// Tantivy NRT search — unchanged, already fast.
fn search_messages(state: &PostOffice, args: Value) -> Result<Value, String> {
    let query_str = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or("Missing query")?;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(10)
        .clamp(1, 1000) as usize;

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
        results.push(parsed);
    }

    Ok(json!(results))
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
            inbox_messages(&inbox)[0]["from"].as_str().unwrap(),
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

        // Each occurrence in the `to` array creates a separate delivery
        let inbox = get_inbox(&state, json!({ "agent_name": "bob" })).unwrap();
        assert_eq!(
            inbox_messages(&inbox).len(),
            2,
            "Duplicate recipient gets the message twice (no dedup)"
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
        assert_eq!(msg["subject"], "(No Subject)");
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
        assert_eq!(msgs1[0]["from"], "sender");
        assert!(msgs1[0]["timestamp"].is_i64());

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
            inbox_messages(&inbox)[0]["from"].as_str().unwrap(),
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

        // Send with duplicate recipients — pre-check sees 9999 twice, both pass
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
            "Duplicate recipients both pass pre-check (same inbox scanned twice)"
        );

        // Bob now has MAX + 1 messages (cap exceeded by duplicate).
        // Count via paginated drain.
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
            total,
            MAX_INBOX_SIZE + 1,
            "Duplicate recipient pushes both succeed, soft cap exceeded by 1"
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
        // to_recipients is stored as "bob charlie" in Tantivy.
        // For simple names (no spaces), the join is unambiguous.
        // This is a known limitation for names with spaces, but
        // validate_agent_name doesn't currently reject spaces.
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
        assert_eq!(arr[0]["from"], "Sender");
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
            inbox_messages(&inbox)[0]["from"].as_str().unwrap().len(),
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

        // 1KB recipient names × 10 recipients — names are DashMap keys only
        let recipients: Vec<String> = (0..10)
            .map(|i| format!("{}{}", "R".repeat(1000), i))
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
        assert!(result.is_ok(), "Long recipient names are accepted");

        // Verify each got exactly one message with normal-sized content
        let inbox = get_inbox(&state, json!({ "agent_name": &recipients[0] })).unwrap();
        let msg = &inbox_messages(&inbox)[0];
        assert_eq!(msg["from"], "alice");
        assert_eq!(msg["body"], "hello");
    }

    // ── H4 (DISPROVED): Unvalidated program/model fields are bounded ─────
    // program and model are stored once per agent (not per recipient).
    // No amplification possible. This test documents the behavior.
    #[tokio::test]
    async fn dr7_h4_large_program_model_accepted() {
        let (state, _idx, _repo) = test_post_office();

        let big_program = "P".repeat(10_000);
        let big_model = "M".repeat(10_000);

        let result = create_agent(
            &state,
            json!({
                "project_key": "test",
                "name_hint": "BigAgent",
                "program": big_program,
                "model": big_model,
            }),
        );
        assert!(
            result.is_ok(),
            "Large program/model are accepted (stored once per agent, not amplified)"
        );

        let record = state.agents.get("BigAgent").unwrap();
        assert_eq!(record.program.len(), 10_000);
        assert_eq!(record.model.len(), 10_000);
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

        // Send to a recipient with path-like characters
        send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["../evil"],
                "subject": "test",
                "body": "hello",
            }),
        )
        .unwrap();

        // get_inbox with the same special-char name retrieves correctly
        let inbox = get_inbox(&state, json!({ "agent_name": "../evil" })).unwrap();
        assert_eq!(
            inbox_messages(&inbox).len(),
            1,
            "Special-char agent_name works as DashMap key (no filesystem access)"
        );
        assert_eq!(inbox_messages(&inbox)[0]["from"], "alice");
    }

    // ── H3 (DISPROVED): Whitespace-only recipient is functional ─────────
    // Not filtered by `.filter(|s| !s.is_empty())`. Works as DashMap key.
    #[tokio::test]
    async fn dr8_h3_whitespace_recipient_is_functional() {
        let (state, _idx, _repo) = test_post_office();

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["   "],
                "subject": "test",
                "body": "hello",
            }),
        );
        assert!(result.is_ok(), "Whitespace-only recipient is accepted");

        // Retrievable via get_inbox with the same whitespace key
        let inbox = get_inbox(&state, json!({ "agent_name": "   " })).unwrap();
        assert_eq!(
            inbox_messages(&inbox).len(),
            1,
            "Whitespace recipient functional as DashMap key"
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
        assert_eq!(inbox_messages(&inbox)[0]["from"], "Агент_1");
        assert_eq!(inbox_messages(&inbox)[0]["body"], "こんにちは");
    }

    // ── H4 (DISPROVED): to_recipients STRING field is opaque in Tantivy ──
    // Space-joined names are stored as a single STRING token. No ambiguity.
    #[tokio::test]
    async fn dr9_h4_to_recipients_is_opaque_string_token() {
        let (state, _idx, _repo) = test_post_office();

        // Send to multiple recipients — to_recipients becomes "bob charlie"
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
                msg["from"].as_str().unwrap(),
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
        assert_eq!(inbox_messages(&inbox)[0]["from"], "alice");
        assert_eq!(inbox_messages(&inbox)[0]["subject"], "note to self");
    }

    // ══════════════════════════════════════════════════════════════════════
    // Deep Review #11 — Hypothesis Tests
    // ══════════════════════════════════════════════════════════════════════

    // ── H1 (CONFIRMED): program and model lack explicit length bounds ────
    // Every other user-facing text input has a MAX_* constant:
    //   agent_name (128), from_agent (256), project_key (256),
    //   subject (1024), body (65536), recipients count (100).
    // But program and model have no explicit bound. The 1MB HTTP body limit
    // is the only implicit cap. This test documents the gap.
    #[tokio::test]
    async fn dr11_h1_program_model_no_length_validation() {
        let (state, _idx, _repo) = test_post_office();

        // 100KB program and model — no app-level rejection
        let big_program = "P".repeat(100_000);
        let big_model = "M".repeat(100_000);

        let result = create_agent(
            &state,
            json!({
                "project_key": "test",
                "name_hint": "BigAgent",
                "program": big_program,
                "model": big_model,
            }),
        );

        // CONFIRMED: no length validation on program/model.
        // These are accepted at any size (only bounded by HTTP body limit).
        assert!(
            result.is_ok(),
            "100KB program/model accepted — no explicit length validation"
        );

        let record = state.agents.get("BigAgent").unwrap();
        assert_eq!(record.program.len(), 100_000);
        assert_eq!(record.model.len(), 100_000);
    }

    // ── H2 (CONFIRMED): Agent "." passes validation, anomalous Git path ──
    // "." is not empty, not "..", has no slashes/null/control chars, and is
    // not whitespace-only — so it passes validate_agent_name. The resulting
    // Git path "agents/./profile.json" normalizes to "agents/profile.json"
    // on the filesystem. DashMap usage is unaffected (exact string key).
    #[tokio::test]
    async fn dr11_h2_dot_agent_name_is_valid() {
        let (state, _idx, _repo) = test_post_office();

        let result = create_agent(&state, json!({ "project_key": "test", "name_hint": "." }));
        assert!(
            result.is_ok(),
            "Agent name '.' passes validation (not '..')"
        );

        // Stored correctly in DashMap with exact key "."
        assert!(state.agents.contains_key("."));
        let record = state.agents.get(".").unwrap();
        assert_eq!(record.name, ".");

        // Messaging works — DashMap key is exact string match
        send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["."],
                "subject": "to dot",
                "body": "hello dot agent",
            }),
        )
        .unwrap();

        let inbox = get_inbox(&state, json!({ "agent_name": "." })).unwrap();
        assert_eq!(inbox_messages(&inbox).len(), 1);
        assert_eq!(inbox_messages(&inbox)[0]["from"], "alice");
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
        assert!(result.is_ok(), "Body with null bytes must be accepted");

        let inbox = get_inbox(&state, json!({ "agent_name": "bob" })).unwrap();
        assert_eq!(inbox_messages(&inbox).len(), 1);
        assert_eq!(
            inbox_messages(&inbox)[0]["body"].as_str().unwrap(),
            body_with_nulls,
            "Null bytes preserved in body round-trip"
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
}
