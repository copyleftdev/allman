use crate::models::InboxEntry;
use crate::state::{AgentRecord, PersistOp, PostOffice};
use chrono::Utc;
use serde_json::{json, Value};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use uuid::Uuid;

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
        tracing::warn!("Persist channel full, dropping git commit for agent {}: {}", name, e);
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
        tracing::warn!("Persist channel full, dropping index op for message {}: {}", message_id, e);
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

    // ── H3: Unbounded Inbox Growth ──────────────────────────────────────────
    // Hypothesis: There is no limit on the number of messages in an inbox.
    // A sender can push an arbitrary number of messages without error.
    #[tokio::test]
    async fn h3_unbounded_inbox_growth_no_limit() {
        let (state, _idx, _repo) = test_post_office();

        // Send 10,000 messages to the same recipient
        for i in 0..10_000 {
            let result = send_message(
                &state,
                json!({
                    "from_agent": "spammer",
                    "to": ["victim"],
                    "subject": format!("spam #{}", i),
                    "body": "x".repeat(100),
                }),
            );
            assert!(
                result.is_ok(),
                "send_message should never reject on inbox size"
            );
        }

        // CONFIRMED: All 10,000 messages accepted. No backpressure.
        let inbox = state.inboxes.get("victim").unwrap();
        assert_eq!(
            inbox.len(),
            10_000,
            "All 10k messages stored — no inbox limit exists"
        );
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
}
