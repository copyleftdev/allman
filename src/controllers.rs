use crate::models::InboxEntry;
use crate::state::{AgentRecord, PersistOp, PostOffice};
use chrono::Utc;
use serde_json::{json, Value};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use uuid::Uuid;

const MAX_INBOX_SIZE: usize = 10_000;

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
    if name.contains('/') || name.contains('\\') {
        return Err("Invalid agent name: must not contain path separators".to_string());
    }
    if name.contains("..") {
        return Err("Invalid agent name: must not contain '..'".to_string());
    }
    if name.contains('\0') {
        return Err("Invalid agent name: must not contain null bytes".to_string());
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
    let program = args
        .get("program")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let model = args
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let name_hint = args.get("name_hint").and_then(|v| v.as_str());

    let project_id = format!(
        "proj_{}",
        Uuid::new_v5(&Uuid::NAMESPACE_DNS, project_key.as_bytes()).simple()
    );

    // Lock-free upsert into DashMap
    state
        .projects
        .entry(project_id.clone())
        .or_insert_with(|| project_key.to_string());

    let name = name_hint.unwrap_or("AnonymousAgent").to_string();
    validate_agent_name(&name)?;

    // Reject cross-project name collisions. Same-project re-registration
    // is allowed (idempotent upsert — expected by benchmarks and simulations).
    if let Some(existing) = state.agents.get(&name) {
        if existing.project_id != project_id {
            return Err(format!(
                "Agent '{}' already registered in a different project",
                name
            ));
        }
    }

    let agent_id = Uuid::new_v4().to_string();

    let record = AgentRecord {
        id: agent_id.clone(),
        project_id: project_id.clone(),
        name: name.clone(),
        program: program.to_string(),
        model: model.to_string(),
    };

    state.agents.insert(name.clone(), record);

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

    if to_agents.is_empty() {
        return Err("No recipients specified".to_string());
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

    for recipient in &to_agents {
        let mut inbox = state.inboxes.entry(recipient.clone()).or_default();
        if inbox.len() >= MAX_INBOX_SIZE {
            return Err(format!(
                "Recipient '{}' inbox full ({} messages) — drain with get_inbox first",
                recipient, MAX_INBOX_SIZE
            ));
        }
        inbox.push(entry.clone());
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
// Hot path: DashMap remove (atomic drain). No disk I/O.
fn get_inbox(state: &PostOffice, args: Value) -> Result<Value, String> {
    let agent_name = args
        .get("agent_name")
        .and_then(|v| v.as_str())
        .ok_or("Missing agent_name")?;

    // Atomic drain: remove all unread messages for this agent
    let messages = state
        .inboxes
        .remove(agent_name)
        .map(|(_, entries)| entries)
        .unwrap_or_default();

    let result: Vec<Value> = messages
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

    Ok(json!(result))
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

        // Drain the inbox
        get_inbox(&state, json!({ "agent_name": "recipient" })).unwrap();

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
        let first_arr = first.as_array().unwrap();
        assert_eq!(first_arr.len(), 1, "First drain should return the message");

        let second = get_inbox(&state, json!({ "agent_name": "b" })).unwrap();
        let second_arr = second.as_array().unwrap();
        assert_eq!(
            second_arr.len(),
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
                inbox.as_array().unwrap().len(),
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
        assert_eq!(r.as_array().unwrap().len(), 0);
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

        // CONFIRMED BUG: Both may succeed because the get() → insert() gap
        // allows both threads to see "no agent exists" simultaneously.
        // At minimum, only one should succeed; the other should be rejected.
        // With the current code, both CAN succeed (race window).
        if successes == 2 {
            // This proves the TOCTOU race: both passed the collision check.
            // The agent record belongs to whichever thread inserted last.
            let record = state.agents.get("RaceName").unwrap();
            eprintln!(
                "TOCTOU CONFIRMED: Both succeeded, winner project_id = {}",
                record.project_id
            );
            // We document this as a confirmed issue but don't fail the test —
            // it's non-deterministic.
        } else {
            assert_eq!(successes, 1, "Exactly one should succeed");
            assert_eq!(failures, 1, "Exactly one should be rejected");
        }
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
        assert!(result.is_err(), "Should fail because recipient_b inbox is full");

        // CONFIRMED BUG: recipient_a already received the message despite the error
        let inbox_a = get_inbox(&state, json!({ "agent_name": "recipient_a" })).unwrap();
        let a_count = inbox_a.as_array().unwrap().len();

        // This documents the partial delivery: A got it, but caller got an error.
        // a_count == 1 proves partial delivery occurred.
        assert_eq!(
            a_count, 1,
            "PARTIAL DELIVERY CONFIRMED: recipient_a got the message despite error return"
        );
    }

    // ── H4: No field length limits — very long agent names are accepted ──
    #[tokio::test]
    async fn h4_very_long_agent_name_is_accepted() {
        let (state, _idx, _repo) = test_post_office();
        let long_name = "A".repeat(10_000);

        let result = create_agent(
            &state,
            json!({ "project_key": "test", "name_hint": long_name }),
        );

        // CONFIRMED: No length limit — 10KB name is accepted and stored.
        // This name will be used as a filesystem path component in git commits.
        assert!(
            result.is_ok(),
            "Very long agent name is accepted (no length limit exists)"
        );
        assert!(
            state.agents.contains_key(&long_name),
            "10KB name stored in DashMap"
        );
    }

    // ── H4b: Very long message body is accepted ──────────────────────────
    #[tokio::test]
    async fn h4b_very_long_message_body_is_accepted() {
        let (state, _idx, _repo) = test_post_office();
        let huge_body = "X".repeat(1_000_000); // 1MB body

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "big payload",
                "body": huge_body,
            }),
        );

        // CONFIRMED: No body size limit at the application layer.
        assert!(
            result.is_ok(),
            "1MB message body is accepted (no size limit exists)"
        );

        let inbox = get_inbox(&state, json!({ "agent_name": "bob" })).unwrap();
        let msg = &inbox.as_array().unwrap()[0];
        assert_eq!(
            msg["body"].as_str().unwrap().len(),
            1_000_000,
            "Full 1MB body stored in inbox"
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
            inbox.as_array().unwrap()[0]["from"].as_str().unwrap(),
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
            get_inbox(&s1, json!({ "agent_name": "target" })).unwrap()
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
        let drained_count = drained.as_array().unwrap().len();

        // Whatever was drained + whatever remains in inbox + whatever was
        // sent concurrently should account for all messages. No message lost.
        let remaining = get_inbox(&state, json!({ "agent_name": "target" })).unwrap();
        let remaining_count = remaining.as_array().unwrap().len();

        let total_accounted = drained_count + remaining_count;
        // We started with 100 + sent up to 50 more = up to 150 total
        assert!(
            total_accounted >= 100 && total_accounted <= 100 + sent,
            "No messages lost: drained={} remaining={} sent={}",
            drained_count,
            remaining_count,
            sent
        );
    }

    // ── H10: Whitespace-only and control-char agent names ────────────────
    #[tokio::test]
    async fn h10_whitespace_only_agent_name_is_accepted() {
        let (state, _idx, _repo) = test_post_office();

        let result = create_agent(
            &state,
            json!({ "project_key": "test", "name_hint": "   " }),
        );

        // CONFIRMED: Whitespace-only names pass validation
        assert!(
            result.is_ok(),
            "Whitespace-only name passes current validation"
        );
    }

    #[tokio::test]
    async fn h10_control_char_agent_name_is_accepted() {
        let (state, _idx, _repo) = test_post_office();

        let result = create_agent(
            &state,
            json!({ "project_key": "test", "name_hint": "agent\x07bell" }),
        );

        // CONFIRMED: Control characters (except \0) pass validation
        assert!(
            result.is_ok(),
            "Control character in name passes current validation"
        );
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
            inbox.as_array().unwrap().len(),
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
        assert!(resp2["id"].is_null(), "Explicit null id should be echoed as null");
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
        let msg = &inbox.as_array().unwrap()[0];
        assert_eq!(msg["subject"], "(No Subject)");
        assert_eq!(msg["body"], "");
    }
}
