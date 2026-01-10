use crate::git_actor::GitRequest;
use crate::state::PostOffice;
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
                "create_agent" => create_agent(&state, args).await,
                "search_messages" => search_messages(&state, args).await,
                "send_message" => send_message(&state, args).await,
                "get_inbox" => get_inbox(&state, args).await,
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

async fn create_agent(state: &PostOffice, args: Value) -> Result<Value, String> {
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
    let task = args
        .get("task_description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let name_hint = args.get("name_hint").and_then(|v| v.as_str());

    // Simple Project ID derivation for MVP (hash of key)
    let project_id = format!(
        "proj_{}",
        Uuid::new_v5(&Uuid::NAMESPACE_DNS, project_key.as_bytes()).simple()
    );

    // Upsert project
    sqlx::query(
        "INSERT OR IGNORE INTO projects (id, human_key, slug, created_ts, meta) VALUES (?, ?, ?, ?, ?)"
    )
    .bind(&project_id)
    .bind(project_key)
    .bind(&project_id)
    .bind(Utc::now().timestamp())
    .bind("{}")
    .execute(&state.db)
    .await
    .map_err(|e| format!("DB Error: {}", e))?;

    let name = name_hint.unwrap_or("AnonymousAgent").to_string();
    let agent_id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();

    sqlx::query(
        "INSERT INTO agents (id, project_id, name, program, model, inception_ts, task, last_active_ts) 
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(&agent_id)
    .bind(&project_id)
    .bind(&name)
    .bind(program)
    .bind(model)
    .bind(now)
    .bind(task)
    .bind(now)
    .execute(&state.db)
    .await
    .map_err(|e| format!("Failed to register agent: {}", e))?;

    // Commit to Git
    let profile = json!({ 
        "id": agent_id, 
        "project_id": project_id,
        "name": name, 
        "program": program, 
        "model": model 
    });
    let path = format!("agents/{}/profile.json", name);
    state
        .git_sender
        .send(GitRequest::CommitFile {
            path: path.clone(),
            content: serde_json::to_string_pretty(&profile).unwrap(),
            message: format!("Register registered agent {}", name),
        })
        .await
        .map_err(|_| "Failed to queue git commit")?;

    Ok(json!(profile))
}

async fn search_messages(state: &PostOffice, args: Value) -> Result<Value, String> {
    let query_str = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or("Missing query")?;
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(10) as usize;

    let index = &state.index;
    // NRT Search: Use cached reader
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
        results.push(serde_json::from_str::<Value>(&doc_json).unwrap());
    }

    Ok(json!(results))
}

async fn send_message(state: &PostOffice, args: Value) -> Result<Value, String> {
    let from_agent = args
        .get("from_agent")
        .and_then(|v| v.as_str())
        .ok_or("Missing from_agent")?;
    let to_agents = args
        .get("to")
        .and_then(|v| v.as_array())
        .ok_or("Missing to recipients")?
        .iter()
        .map(|v| v.as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    let subject = args
        .get("subject")
        .and_then(|v| v.as_str())
        .unwrap_or("(No Subject)");
    let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let project_id = args
        .get("project_id")
        .and_then(|v| v.as_str())
        .unwrap_or(""); // Optional if we infer contexts

    if to_agents.is_empty() {
        return Err("No recipients specified".to_string());
    }

    let message_id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();
    let thread_id = message_id.clone(); // For now, new thread

    // 1. DB Insert
    sqlx::query(
        "INSERT INTO messages (id, project_id, thread_id, subject, body_md, from_agent, created_ts, importance, ack_required)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0)"
    )
    .bind(&message_id)
    .bind(project_id)
    .bind(&thread_id)
    .bind(subject)
    .bind(body)
    .bind(from_agent)
    .bind(now)
    .bind("normal")
    .execute(&state.db)
    .await
    .map_err(|e| format!("Failed to insert message: {}", e))?;

    // Recipients
    for recipient in &to_agents {
        sqlx::query(
            "INSERT INTO message_recipients (message_id, agent_name, kind, read_ts, ack_ts) VALUES (?, ?, 'to', NULL, NULL)"
        )
        .bind(&message_id)
        .bind(recipient)
        .execute(&state.db)
        .await
        .map_err(|e| format!("Failed to add recipient {}: {}", recipient, e))?;
    }

    // 2. Git Commit (Mailbox)
    // We strive for a structure like `mailboxes/{agent_name}/inbox/{message_id}.md`
    // And `mailboxes/{sender}/sent/{message_id}.md`
    let filename = format!("{}_{}.md", now, message_id);
    let content = format!(
        "---\nid: {}\nfrom: {}\nto: {:?}\nsubject: {}\ndate: {}\n---\n\n{}",
        message_id, from_agent, to_agents, subject, now, body
    );

    // Write to sender's sent box
    state
        .git_sender
        .send(GitRequest::CommitFile {
            path: format!("agents/{}/sent/{}", from_agent, filename),
            content: content.clone(),
            message: format!("Sent message: {}", subject),
        })
        .await
        .ok();

    // Write to recipients' inboxes
    for recipient in &to_agents {
        state
            .git_sender
            .send(GitRequest::CommitFile {
                path: format!("agents/{}/inbox/{}", recipient, filename),
                content: content.clone(),
                message: format!("Message from {} to {}", from_agent, recipient),
            })
            .await
            .ok();
    }

    // 3. Tantivy Indexing
    {
        let mut writer = state
            .index_writer
            .lock()
            .map_err(|_| "Failed to lock index writer")?;
        let schema = state.index.schema();
        let f_id = schema.get_field("id").unwrap();
        let f_project = schema.get_field("project_id").unwrap();
        let f_from = schema.get_field("from_agent").unwrap();
        let f_to = schema.get_field("to_recipients").unwrap();
        let f_subject = schema.get_field("subject").unwrap();
        let f_body = schema.get_field("body").unwrap();
        let f_ts = schema.get_field("created_ts").unwrap();

        let mut doc = tantivy::Document::default();
        doc.add_text(f_id, &message_id);
        doc.add_text(f_project, project_id);
        doc.add_text(f_from, from_agent);
        doc.add_text(f_to, to_agents.join(" "));
        doc.add_text(f_subject, subject);
        doc.add_text(f_body, body);
        doc.add_i64(f_ts, now);

        writer.add_document(doc).map_err(|e| e.to_string())?;
        // writer.commit().map_err(|e| e.to_string())?; // Batch commit
    }

    Ok(json!({ "id": message_id, "status": "sent" }))
}

async fn get_inbox(state: &PostOffice, args: Value) -> Result<Value, String> {
    let agent_name = args.get("agent_name").and_then(|v| v.as_str()).ok_or("Missing agent_name")?;
    
    // Fetch unread messages
    let rows = sqlx::query(
        "SELECT m.id, m.from_agent, m.subject, m.body_md, m.created_ts 
         FROM messages m
         JOIN message_recipients r ON m.id = r.message_id
         WHERE r.agent_name = ? AND r.kind = 'to' AND r.read_ts IS NULL
         ORDER BY m.created_ts ASC"
    )
    .bind(agent_name)
    .fetch_all(&state.db)
    .await
    .map_err(|e| format!("DB Error: {}", e))?;

    let mut messages = Vec::new();
    for row in rows {
        use sqlx::Row;
        let id: String = row.get("id");
        let from: String = row.get("from_agent");
        let subject: String = row.get("subject");
        let body: String = row.get("body_md");
        let ts: i64 = row.get("created_ts");

        // Mark as read (Side-effect! But useful for this simple sim)
        // In a real system, explicit 'ack' is better.
        let _ = sqlx::query("UPDATE message_recipients SET read_ts = ? WHERE message_id = ? AND agent_name = ?")
            .bind(Utc::now().timestamp())
            .bind(&id)
            .bind(agent_name)
            .execute(&state.db)
            .await;

        messages.push(json!({
            "id": id,
            "from": from,
            "subject": subject,
            "body": body,
            "timestamp": ts
        }));
    }

    Ok(json!(messages))
}
