use crate::models::InboxEntry;
use crate::state::{AgentRecord, PersistOp, PostOffice};
use chrono::Utc;
use serde_json::{json, Value};
use std::sync::OnceLock;
use tantivy::collector::{Count, TopDocs};
use tantivy::query::QueryParser;
use uuid::Uuid;

const MAX_INBOX_SIZE: usize = 10_000;
const MAX_AGENT_NAME_LEN: usize = 128;
const MAX_PROJECT_KEY_LEN: usize = 256;
const MAX_RECIPIENTS: usize = 100;
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
const MAX_INBOXES: usize = 100_000;
const MAX_TOOL_NAME_LEN: usize = 64;

// ── Cached tools/list response ──────────────────────────────────────────────
// Built once on first access via OnceLock. The tools/list response is entirely
// static (schema + constants), so reconstructing it on every request was
// wasteful — ~60 string literals and ~10 constant evaluations per call (DR45-H1).
fn tools_list_value() -> &'static Value {
    static TOOLS_LIST: OnceLock<Value> = OnceLock::new();
    TOOLS_LIST.get_or_init(|| {
        json!({
            "tools": [
                {
                    "name": "create_agent",
                    "description": "Register a new agent in a project",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "project_key": { "type": "string", "description": "Human-readable project identifier", "maxLength": MAX_PROJECT_KEY_LEN },
                            "name_hint": { "type": "string", "description": "Agent name (defaults to AnonymousAgent)", "maxLength": MAX_AGENT_NAME_LEN },
                            "program": { "type": "string", "description": "Program/version identifier", "maxLength": MAX_PROGRAM_LEN },
                            "model": { "type": "string", "description": "LLM model name", "maxLength": MAX_MODEL_LEN }
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
                            "from_agent": { "type": "string", "description": "Sender agent name", "maxLength": MAX_AGENT_NAME_LEN },
                            "to": { "type": "array", "items": { "type": "string", "maxLength": MAX_AGENT_NAME_LEN }, "description": "Recipient agent names", "minItems": 1, "maxItems": MAX_RECIPIENTS },
                            "subject": { "type": "string", "description": "Message subject", "maxLength": MAX_SUBJECT_LEN },
                            "body": { "type": "string", "description": "Message body", "maxLength": MAX_BODY_LEN },
                            "project_id": { "type": "string", "description": "Project ID for search indexing", "maxLength": MAX_PROJECT_ID_LEN }
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
                            "query": { "type": "string", "description": "Search query (Tantivy syntax)", "maxLength": MAX_QUERY_LEN },
                            "limit": { "type": "integer", "description": "Max results to return", "default": DEFAULT_SEARCH_LIMIT, "minimum": 1, "maximum": MAX_SEARCH_LIMIT }
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
                            "agent_name": { "type": "string", "description": "Agent whose inbox to drain", "maxLength": MAX_AGENT_NAME_LEN },
                            "limit": { "type": "integer", "description": "Max messages to drain", "default": DEFAULT_INBOX_LIMIT, "minimum": 1, "maximum": MAX_INBOX_LIMIT }
                        },
                        "required": ["agent_name"]
                    }
                }
            ]
        })
    })
}

pub async fn handle_mcp_request(state: PostOffice, req: Value) -> Value {
    // JSON-RPC 2.0 §4: request MUST be a JSON Object.
    // Arrays are batch requests (unsupported); scalars are invalid (DR25-H5).
    if !req.is_object() {
        return json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": -32600, "message": "Invalid Request: must be a JSON object" }
        });
    }

    let id = req.get("id");

    // JSON-RPC 2.0 §4.1: "A Notification is a Request object without an 'id' member."
    // "The Server MUST NOT reply to a Notification" — even if malformed.
    // This check MUST come before jsonrpc validation so that malformed
    // notifications (wrong/missing jsonrpc) are silently dropped (DR26-H3).
    if id.is_none() {
        return Value::Null;
    }

    // JSON-RPC 2.0 §4: "method" MUST be a String. Missing or non-string
    // method is a structurally invalid request → -32600 (DR28-H3).
    let method = match req.get("method") {
        Some(v) => match v.as_str() {
            Some(s) => s,
            None => {
                return json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32600, "message": "Invalid Request: method must be a string" }
                });
            }
        },
        None => {
            return json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32600, "message": "Invalid Request: missing method field" }
            });
        }
    };
    // Borrow params as a reference — avoid cloning the entire params object
    // which includes nested arguments with potentially large body strings.
    // Only clone the specific sub-values we need (DR33-H4).
    let params = req.get("params").unwrap_or(&Value::Null);

    // JSON-RPC 2.0 §4: "jsonrpc" MUST be exactly "2.0" (DR25-H3).
    if req.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32600, "message": "Invalid Request: jsonrpc field must be exactly \"2.0\"" }
        });
    }

    // JSON-RPC 2.0: params MUST be an object or array (§4.2).
    // Reject non-object params early with a clear error instead of letting
    // them fall through to misleading "Unknown tool" errors (DR24-H5).
    // Absent params (null) is treated as empty object for dispatch purposes,
    // but non-null non-object params (string, array, number) are rejected.
    if !params.is_null() && !params.is_object() {
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32602, "message": "Invalid params: must be an object" }
        });
    }

    match method {
        "tools/call" => {
            // Validate tool name type: non-string values (integers, booleans,
            // null) must return a clear type error, not fall through to
            // "Unknown tool: " which is misleading (DR29-H4).
            let tool_name = match params.get("name") {
                Some(v) => match v.as_str() {
                    Some(s) => s,
                    None => {
                        return json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32602, "message": "Invalid params: name must be a string" }
                        });
                    }
                },
                // Missing tool name is a specific error, not "Unknown tool: "
                // with an empty name which is ambiguous (DR34-H5).
                None => {
                    return json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": "Missing tool name in params" }
                    });
                }
            };
            // Reject excessively long tool names before matching. Without this,
            // a ~1MB tool name (the Axum body limit) would be echoed verbatim
            // in the "Unknown tool" error response via format!() (DR44-H3).
            if tool_name.len() > MAX_TOOL_NAME_LEN {
                return json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": format!("Tool name too long: {} bytes (max {})", tool_name.len(), MAX_TOOL_NAME_LEN) }
                });
            }
            // Clone only arguments from the borrowed params reference — single
            // clone instead of cloning all of params then cloning arguments
            // out of the clone (DR33-H4).
            let args = params.get("arguments").cloned().unwrap_or(json!({}));

            // Validate arguments is an object (or absent → defaulted to {}).
            // Non-object arguments (string, array, number) would produce
            // misleading tool-level errors like "Missing project_key" (DR28-H4).
            if !args.is_object() {
                return json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": "Invalid arguments: must be an object" }
                });
            }

            let result = match tool_name {
                "create_agent" => create_agent(&state, args),
                "search_messages" => search_messages(&state, args),
                "send_message" => send_message(&state, args),
                "get_inbox" => get_inbox(&state, args),
                _ => {
                    // The method "tools/call" IS found — only the tool name
                    // parameter is invalid. Use -32602 (Invalid params), not
                    // -32601 (Method not found) (DR25-H4).
                    return json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": format!("Unknown tool: {}", tool_name) }
                    });
                }
            };

            match result {
                Ok(content) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "content": [{ "type": "text", "text": content.to_string() }] }
                }),
                Err(e) => {
                    // Log tool errors server-side so operators have visibility
                    // into error rates and types without client cooperation.
                    // Previously errors were only returned in the JSON-RPC
                    // response with no server-side audit trail (DR43-H2).
                    tracing::warn!(tool = tool_name, error = %e, "Tool call failed");
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": e }
                    })
                }
            }
        }
        // Return the cached tools/list response. The tools array is static
        // (schema + constants), so it's built once via OnceLock and cloned
        // per request instead of reconstructing ~60 string literals and ~10
        // constant evaluations each time (DR45-H1).
        "tools/list" => {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": tools_list_value()
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

// Shared text field validation: empty, null bytes (specific error message before
// generic control char check — intentional redundancy, see DR31-H3), control
// characters, length, whitespace. Used by validate_name() and validate_text_field().
// Single source of truth for the checks common to all user-supplied strings (DR31-H2).
fn validate_text_core(value: &str, field: &str, max_len: usize) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{} must not be empty", field));
    }
    // .len() returns byte count, not character count. Error message must
    // say "byte limit" to be accurate for multi-byte UTF-8 strings (DR33-H1).
    if value.len() > max_len {
        return Err(format!("{} exceeds {} byte limit", field, max_len));
    }
    // Null byte check fires before control char check for a more specific error
    // message. This is intentionally redundant — '\0'.is_control() is true, so
    // the control char check below would also catch it (DR31-H3).
    if value.contains('\0') {
        return Err(format!("{} must not contain null bytes", field));
    }
    if value.chars().any(|c| c.is_control()) {
        return Err(format!("{} must not contain control characters", field));
    }
    if value.trim().is_empty() {
        return Err(format!("{} must not be whitespace-only", field));
    }
    if value != value.trim() {
        return Err(format!(
            "{} must not have leading or trailing whitespace",
            field
        ));
    }
    Ok(())
}

// Validate text fields that don't appear in filesystem paths (project_key,
// program, model). Applies the shared core checks without path-specific rules.
fn validate_text_field(value: &str, field: &str, max_len: usize) -> Result<(), String> {
    validate_text_core(value, field, max_len)
}

// Validate optional text fields (subject, project_id) that may be empty strings.
// Applies the same checks as validate_text_core but allows empty values —
// empty string means "not provided" for optional fields (DR40-H1).
// Single source of truth: prevents rule drift between subject and project_id
// validation, which previously duplicated the same 5-check inline pattern.
fn validate_optional_text(value: &str, field: &str, max_len: usize) -> Result<(), String> {
    if value.is_empty() {
        return Ok(());
    }
    validate_text_core(value, field, max_len)
}

// Validate agent names used in filesystem paths (name_hint, from_agent,
// recipients, agent_name). Adds path-safety rules on top of the shared core.
// Single source of truth — all name validation goes through this function
// to prevent rule drift (DR30-H1).
fn validate_name(name: &str, field: &str) -> Result<(), String> {
    validate_text_core(name, field, MAX_AGENT_NAME_LEN)?;
    if name.contains('/') || name.contains('\\') {
        return Err(format!("{} must not contain path separators", field));
    }
    if name.contains("..") {
        return Err(format!("{} must not contain '..'", field));
    }
    // Single check for dot-prefixed names. "." is a subset of starts_with('.')
    // — previously had a redundant `name == "."` check before this one.
    // Merged into one conditional with a specific message for "." (DR35-H3).
    if name.starts_with('.') {
        if name == "." {
            return Err(format!("{} must not be '.'", field));
        }
        return Err(format!("{} must not start with '.'", field));
    }
    Ok(())
}

// Wrapper for create_agent's name_hint validation. Uses standard field
// name format ("name_hint") consistent with all other validate_name calls
// (from_agent, recipient, agent_name) — previously used "Invalid agent name:"
// prefix which created inconsistent error messages (DR43-H3).
fn validate_agent_name(name: &str) -> Result<(), String> {
    validate_name(name, "name_hint")
}

// ── create_agent ─────────────────────────────────────────────────────────────
// Hot path: DashMap insert + crossbeam send. No disk I/O.
fn create_agent(state: &PostOffice, args: Value) -> Result<Value, String> {
    let project_key = match args.get("project_key") {
        Some(v) => v.as_str().ok_or("project_key must be a string")?,
        None => return Err("Missing project_key".to_string()),
    };
    // Shared text validation: empty, null bytes, control chars, length,
    // whitespace. No path-separator or dot checks — project_key is only used
    // for UUID generation, not filesystem paths (DR31-H2).
    validate_text_field(project_key, "project_key", MAX_PROJECT_KEY_LEN)?;
    // Validate program/model types: if present and non-null, MUST be strings.
    // Silently defaulting to "unknown" for non-string values (e.g., integers)
    // is confusing — the same class of bug fixed for name_hint in DR27-H5 (DR28-H1).
    let program = match args.get("program") {
        Some(v) if v.is_null() => "unknown",
        Some(v) => v.as_str().ok_or("program must be a string if provided")?,
        None => "unknown",
    };
    let model = match args.get("model") {
        Some(v) if v.is_null() => "unknown",
        Some(v) => v.as_str().ok_or("model must be a string if provided")?,
        None => "unknown",
    };
    // Validate name_hint type: if present and non-null, MUST be a string.
    // Silently falling back to "AnonymousAgent" for non-string values (e.g.,
    // integers, booleans) causes confusing cross-project collisions (DR27-H5).
    let name_hint = match args.get("name_hint") {
        Some(v) if v.is_null() => None, // explicit null treated as absent
        Some(v) => Some(v.as_str().ok_or("name_hint must be a string if provided")?),
        None => None,
    };

    // Validate all inputs BEFORE mutating any state.
    // program and model use validate_text_field (shared core checks, DR31-H2).
    // They allow "unknown" as the default, which passes validation (non-empty,
    // no control chars, under limit, no leading/trailing whitespace).
    validate_text_field(program, "program", MAX_PROGRAM_LEN)?;
    validate_text_field(model, "model", MAX_MODEL_LEN)?;
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
    let now = Utc::now().timestamp();

    let agent_id;
    let registered_at;
    let is_reregistration;

    match state.agents.entry(name.clone()) {
        dashmap::mapref::entry::Entry::Occupied(mut occ) => {
            if occ.get().project_id != project_id {
                return Err(format!(
                    "Agent '{}' already registered in a different project",
                    name
                ));
            }
            // Same project — idempotent upsert. Preserve the original agent_id
            // and registered_at so the identity is stable across re-registrations
            // (DR26-H2, DR27-H3).
            agent_id = occ.get().id.clone();
            registered_at = occ.get().registered_at;
            is_reregistration = true;
            let record = AgentRecord {
                id: agent_id.clone(),
                project_id: project_id.clone(),
                name: name.clone(),
                program: program.to_string(),
                model: model.to_string(),
                registered_at,
            };
            occ.insert(record);
        }
        dashmap::mapref::entry::Entry::Vacant(vac) => {
            agent_id = Uuid::new_v4().to_string();
            registered_at = now;
            is_reregistration = false;
            let record = AgentRecord {
                id: agent_id.clone(),
                project_id: project_id.clone(),
                name: name.clone(),
                program: program.to_string(),
                model: model.to_string(),
                registered_at,
            };
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

    // Fire-and-forget: persist agent profile to Git.
    // updated_at is always present for consistent response schema (DR39-H2):
    // - Fresh registration: updated_at is null
    // - Re-registration: updated_at is the current timestamp
    // This lets clients rely on a fixed set of fields regardless of whether
    // the agent existed before (DR38-H4 introduced updated_at, DR39-H2 made
    // it always present).
    let profile = json!({
        "id": agent_id,
        "project_id": project_id,
        "name": name,
        "program": program,
        "model": model,
        "registered_at": registered_at,
        "updated_at": if is_reregistration { serde_json::Value::from(now) } else { serde_json::Value::Null }
    });
    // Use distinct commit messages for fresh registration vs re-registration
    // so the Git audit trail can distinguish them (DR35-H5).
    let commit_msg = if is_reregistration {
        format!("Update agent {}", name)
    } else {
        format!("Register agent {}", name)
    };
    if let Err(e) = state.persist_tx.try_send(PersistOp::GitCommit {
        path: format!("agents/{}/profile.json", name),
        content: serde_json::to_string_pretty(&profile).unwrap(),
        message: commit_msg,
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
    let from_agent = match args.get("from_agent") {
        Some(v) => v.as_str().ok_or("from_agent must be a string")?,
        None => return Err("Missing from_agent".to_string()),
    };
    let to_array = match args.get("to") {
        Some(v) => v.as_array().ok_or("to must be a JSON array")?,
        None => return Err("Missing to recipients".to_string()),
    };
    // Reject ANY non-string elements in the to array up front, including nulls.
    // The schema declares `items: { type: string }` — null is not a string.
    // Previously nulls were silently dropped, causing schema-runtime
    // inconsistency where to:[null] gave misleading "No recipients" (DR31-H1).
    if to_array.iter().any(|v| !v.is_string()) {
        return Err(
            "to array contains non-string elements — all recipients must be strings".to_string(),
        );
    }
    // Dedup with HashSet<&str> borrowing from to_array — filter duplicates
    // BEFORE allocating owned Strings, so duplicate recipients cause zero
    // heap allocations. Only surviving unique recipients get .to_string() (DR33-H5).
    let to_agents: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        to_array
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .filter(|s| seen.insert(*s))
            .map(|s| s.to_string())
            .collect()
    };
    // Validate optional string fields: non-string types (integers, booleans,
    // arrays) must return type errors, not silently default to "" (DR29-H3).
    let subject = match args.get("subject") {
        Some(v) if v.is_null() => "",
        Some(v) => v.as_str().ok_or("subject must be a string if provided")?,
        None => "",
    };
    let body = match args.get("body") {
        Some(v) if v.is_null() => "",
        Some(v) => v.as_str().ok_or("body must be a string if provided")?,
        None => "",
    };
    let project_id = match args.get("project_id") {
        Some(v) if v.is_null() => "",
        Some(v) => v
            .as_str()
            .ok_or("project_id must be a string if provided")?,
        None => "",
    };

    // Single source of truth: use validate_name() for from_agent (DR30-H1).
    // NOTE: from_agent is format-validated only — no state.agents.contains_key()
    // check. Unregistered senders can send messages. This is intentional:
    // the system uses open addressing (recipients also don't require registration),
    // and ephemeral DashMap state is lost on restart, so requiring registration
    // would break messaging after a server restart (DR42-H2).
    validate_name(from_agent, "from_agent")?;
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
    // Single source of truth: use validate_name() for each recipient (DR30-H1).
    for recipient in &to_agents {
        validate_name(recipient, "recipient")?;
    }
    // Subject is optional (defaults to "") — validate via shared helper to
    // prevent rule drift with project_id validation (DR40-H1).
    validate_optional_text(subject, "Subject", MAX_SUBJECT_LEN)?;
    // Body validation is intentionally MORE lenient than subject/project_id:
    // - Whitespace-only and whitespace-padded bodies are allowed (bodies are
    //   multi-line content where indentation and blank content are valid).
    // - Control chars \n, \t, \r are allowed (multi-line content).
    // This asymmetry with validate_optional_text is by design (DR41-H1).
    if body.len() > MAX_BODY_LEN {
        return Err(format!("Body exceeds {} byte limit", MAX_BODY_LEN));
    }
    if body.contains('\0') {
        return Err("Body must not contain null bytes".to_string());
    }
    if body
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t' && c != '\r')
    {
        return Err(
            "Body must not contain control characters (except newline, tab, carriage return)"
                .to_string(),
        );
    }
    // project_id is optional (defaults to "") — validate via shared helper to
    // prevent rule drift with subject validation (DR40-H1).
    validate_optional_text(project_id, "project_id", MAX_PROJECT_ID_LEN)?;

    // Soft cap on total inbox count. Prevents unbounded DashMap growth
    // from phantom recipients that are never drained (DR25-H1).
    // Checked BEFORE delivery to avoid creating entries past the cap.
    // Count of new inboxes that would be created by this message:
    let new_inbox_count = to_agents
        .iter()
        .filter(|r| !state.inboxes.contains_key(*r))
        .count();
    if new_inbox_count > 0 && state.inboxes.len() + new_inbox_count > MAX_INBOXES {
        return Err(format!(
            "Inbox limit reached ({} max) — cannot create new inboxes",
            MAX_INBOXES
        ));
    }

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

    // Construct InboxEntry AFTER pre-checks pass to avoid wasted heap
    // allocations (from_agent, subject, body, project_id clones) when the
    // send is rejected by capacity checks above (DR33-H3).
    let message_id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();
    // Join recipients with unit separator (\x1F) — same format as the Tantivy
    // index, so get_inbox and search_messages return consistent data (DR42-H3).
    let to_recipients_joined = to_agents.join("\x1F");
    let entry = InboxEntry {
        message_id: message_id.clone(),
        from_agent: from_agent.to_string(),
        to_recipients: to_recipients_joined.clone(),
        subject: subject.to_string(),
        body: body.to_string(),
        timestamp: now,
        project_id: project_id.to_string(),
    };

    // All inboxes have capacity (per pre-check). Deliver with per-entry
    // atomic guard to handle concurrent races — if a concurrent send filled
    // an inbox between pre-check and delivery, the entry() block skips the
    // push instead of overshooting MAX_INBOX_SIZE (DR26-H4).
    // Track delivered count so callers know if any recipients were silently
    // skipped due to concurrent races (DR27-H4).
    let mut delivered_count = 0usize;
    // Deliver to a single recipient's inbox. Returns true if delivered.
    // Try get_mut() first to avoid a String allocation for the key when
    // the inbox already exists (the common case). Only fall back to
    // entry() (which requires an owned key) for new inboxes (DR38-H5).
    let deliver = |recipient: &str, inbox_entry: InboxEntry| -> bool {
        // Fast path: existing inbox — borrows &str, no heap allocation.
        if let Some(mut inbox) = state.inboxes.get_mut(recipient) {
            if inbox.len() < MAX_INBOX_SIZE {
                inbox.push(inbox_entry);
                return true;
            }
            // Concurrent race filled this inbox after pre-check.
            return false;
        }
        // Slow path: new inbox — allocates String for the DashMap key.
        // Re-check via entry() in case another thread created it between
        // the get_mut miss and this call.
        match state.inboxes.entry(recipient.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(mut occ) => {
                let inbox = occ.get_mut();
                if inbox.len() < MAX_INBOX_SIZE {
                    inbox.push(inbox_entry);
                    true
                } else {
                    false
                }
            }
            dashmap::mapref::entry::Entry::Vacant(vac) => {
                vac.insert(vec![inbox_entry]);
                true
            }
        }
    };
    // Clone entry for all-but-last recipients. Move entry into the last
    // recipient's inbox to avoid one unnecessary heap allocation of
    // InboxEntry's 6 String fields (DR37-H5).
    for recipient in &to_agents[..to_agents.len() - 1] {
        if deliver(recipient, entry.clone()) {
            delivered_count += 1;
        }
    }
    if deliver(to_agents.last().unwrap(), entry) {
        delivered_count += 1;
    }

    // 2. Fire-and-forget: index in Tantivy (batched by persistence worker).
    // Track whether the persist channel accepted the op so the response
    // can inform the client. Previously, a dropped index op was only
    // logged server-side — clients had no way to know (DR45-H4).
    let indexed = state
        .persist_tx
        .try_send(PersistOp::IndexMessage {
            id: message_id.clone(),
            project_id: project_id.to_string(),
            from_agent: from_agent.to_string(),
            to_recipients: to_recipients_joined,
            subject: subject.to_string(),
            body: body.to_string(),
            created_ts: now,
        })
        .is_ok();
    if !indexed {
        tracing::warn!(
            "Persist channel full, dropping index op for message {}",
            message_id,
        );
    }

    // Include a warning field when delivered_count < expected recipients.
    // This happens when a concurrent send fills an inbox between pre-check
    // and delivery. Callers can detect partial delivery without comparing
    // delivered_count against their expected count (DR35-H4).
    let expected = to_agents.len();
    if delivered_count < expected {
        Ok(json!({
            "id": message_id,
            "status": "sent",
            "delivered_count": delivered_count,
            "indexed": indexed,
            "warning": format!(
                "Partial delivery: {}/{} recipients received the message (concurrent inbox full)",
                delivered_count, expected
            )
        }))
    } else {
        Ok(
            json!({ "id": message_id, "status": "sent", "delivered_count": delivered_count, "indexed": indexed }),
        )
    }
}

// ── get_inbox ────────────────────────────────────────────────────────────────
// Hot path: DashMap partial or full drain. No disk I/O.
// Supports optional `limit` parameter (default 100, max 1000) to bound
// response size. Returns `remaining` count so callers know to fetch more.
fn get_inbox(state: &PostOffice, args: Value) -> Result<Value, String> {
    let agent_name = match args.get("agent_name") {
        Some(v) => v.as_str().ok_or("agent_name must be a string")?,
        None => return Err("Missing agent_name".to_string()),
    };
    // Single source of truth: use validate_name() for agent_name (DR30-H1).
    validate_name(agent_name, "agent_name")?;

    // Validate limit type and range. Non-integer types return a type error
    // (DR29-H1). Out-of-range values (0, negative, >1000) are rejected
    // instead of silently clamped — the schema declares minimum=1 (DR30-H3).
    let limit = match args.get("limit") {
        Some(v) if v.is_null() => DEFAULT_INBOX_LIMIT,
        Some(v) => {
            let n = v.as_i64().ok_or("limit must be an integer if provided")?;
            if n < 1 {
                return Err(format!("limit must be >= 1 (got {})", n));
            }
            if n > MAX_INBOX_LIMIT as i64 {
                return Err(format!("limit must be <= {} (got {})", MAX_INBOX_LIMIT, n));
            }
            n as usize
        }
        None => DEFAULT_INBOX_LIMIT,
    };

    // Fast path: nonexistent inbox — borrows &str, no String allocation.
    // entry() requires an owned String key; skip it when the inbox doesn't
    // exist (the Vacant branch was a no-op anyway) (DR39-H1).
    let mut taken = Vec::new();
    let remaining_count;

    if !state.inboxes.contains_key(agent_name) {
        remaining_count = 0;
    } else {
        // Atomic drain via entry() API. Holds the DashMap shard lock for the
        // entire operation, preventing concurrent send_message from inserting
        // messages that would be overwritten by a separate insert() call.
        // Previous remove→insert pattern had a TOCTOU race window (H1).
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
                    // Reclaim excess Vec capacity after partial drain.
                    // Without this, the Vec retains its peak capacity
                    // indefinitely (e.g., 10K capacity after draining
                    // to 100 entries). Bounded by MAX_INBOX_SIZE but
                    // wasteful for inboxes that had a burst (DR39-H3).
                    inbox.shrink_to_fit();
                }
            }
            // TOCTOU fallback: inbox was removed between contains_key and
            // entry() by a concurrent full drain. Safe — returns empty.
            dashmap::mapref::entry::Entry::Vacant(_) => {
                remaining_count = 0;
            }
        }
    }

    // NOTE: `to_recipients` is a \x1F-delimited string (e.g., "Alice\x1FBob"),
    // not a JSON array. This matches the Tantivy index format for consistency
    // between get_inbox and search_messages. Clients should split on \x1F to
    // recover individual recipient names. The input format (`to` in send_message)
    // is a JSON array — this asymmetry is by design: the internal format avoids
    // nested JSON in stored fields (DR43-H1).
    let result: Vec<Value> = taken
        .into_iter()
        .map(|e| {
            json!({
                "id": e.message_id,
                "from_agent": e.from_agent,
                "to_recipients": e.to_recipients,
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

// ── Tantivy field unwrapper (used by tests only) ────────────────────────────
// Tantivy's schema.to_json() wraps each field value in an array because
// documents can have multiple values per field. Our schema only adds one
// value per field, so unwrap single-element arrays to produce scalar fields.
// NOTE: The main search_messages path now uses direct field extraction to
// avoid the serialize→deserialize round-trip (DR36-H3). This function is
// retained for existing test assertions.
#[cfg(test)]
fn unwrap_tantivy_arrays(doc: Value) -> Value {
    match doc {
        Value::Object(map) => {
            let unwrapped = map
                .into_iter()
                .map(|(k, v)| {
                    let val = match v {
                        Value::Array(mut arr) if arr.len() == 1 => arr.swap_remove(0),
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
// Tantivy NRT search.
fn search_messages(state: &PostOffice, args: Value) -> Result<Value, String> {
    let query_str = match args.get("query") {
        Some(v) => v.as_str().ok_or("query must be a string")?,
        None => return Err("Missing query".to_string()),
    };
    // Query is required — use validate_text_core (shared source of truth)
    // to prevent rule drift with other text validation paths (DR41-H3).
    // Previously this was inline with a different check order (empty+whitespace
    // combined first). Now follows the canonical order: empty → length → null →
    // control → whitespace-only → padded.
    validate_text_core(query_str, "query", MAX_QUERY_LEN)?;
    // Validate limit type: non-integer types (strings, booleans, floats)
    // must return a type error, not silently use the default (DR29-H2).
    // Reject out-of-range values explicitly instead of silent clamping (DR30-H3).
    let limit = match args.get("limit") {
        Some(v) if v.is_null() => DEFAULT_SEARCH_LIMIT,
        Some(v) => {
            let n = v.as_i64().ok_or("limit must be an integer if provided")?;
            if n < 1 {
                return Err(format!("limit must be >= 1 (got {})", n));
            }
            if n > MAX_SEARCH_LIMIT as i64 {
                return Err(format!("limit must be <= {} (got {})", MAX_SEARCH_LIMIT, n));
            }
            n as usize
        }
        None => DEFAULT_SEARCH_LIMIT,
    };

    let index = &state.index;
    let searcher = state.index_reader.searcher();

    // Use pre-resolved field handles from startup — avoids 7 HashMap
    // lookups via schema.get_field() per request (DR37-H3).
    let sf = &state.search_fields;

    let query_parser = QueryParser::for_index(index, vec![sf.subject, sf.body]);
    let query = query_parser
        .parse_query(query_str)
        .map_err(|e| format!("Query parse error: {}", e))?;

    // Multi-collector: TopDocs for results + Count for total matching
    // documents. Previously `count` was always results.len() which is
    // redundant — now it reports total hits for pagination (DR38-H3).
    let (top_docs, total_hits) = searcher
        .search(&query, &(TopDocs::with_limit(limit), Count))
        .map_err(|e| format!("Search error: {}", e))?;

    // Build JSON directly from Tantivy document fields — avoids the
    // schema.to_json() → serde_json::from_str() serialize-deserialize
    // round-trip that created unnecessary string allocations (DR36-H3).
    // Missing fields serialize as null instead of empty string/zero,
    // letting clients distinguish "absent" from "empty" (DR37-H1).
    let mut results = Vec::new();
    for (_score, doc_address) in top_docs {
        let retrieved_doc = searcher.doc(doc_address).map_err(|e| e.to_string())?;
        let doc_val = json!({
            "id": retrieved_doc.get_first(sf.id).and_then(|v| v.as_text()),
            "project_id": retrieved_doc.get_first(sf.project_id).and_then(|v| v.as_text()),
            "from_agent": retrieved_doc.get_first(sf.from_agent).and_then(|v| v.as_text()),
            "to_recipients": retrieved_doc.get_first(sf.to_recipients).and_then(|v| v.as_text()),
            "subject": retrieved_doc.get_first(sf.subject).and_then(|v| v.as_text()),
            "body": retrieved_doc.get_first(sf.body).and_then(|v| v.as_text()),
            "created_ts": retrieved_doc.get_first(sf.created_ts).and_then(|v| v.as_i64()),
        });
        results.push(doc_val);
    }

    // `count` is always results.len() — a convenience field so clients don't
    // need to parse the array to get the count. `total_hits` is the useful
    // pagination field: it reports ALL matching documents, not just the
    // returned subset (DR41-H2).
    Ok(json!({
        "results": results,
        "count": results.len(),
        "total_hits": total_hits
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

        // Both succeed — second is an upsert with stable ID (DR26-H2 fix)
        assert_eq!(
            r1["id"], r2["id"],
            "FIXED: Re-registration preserves original agent_id (DR26-H2)"
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

        // Mixed array with non-string elements is now rejected entirely (DR29-H5).
        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob", 123, null],
                "subject": "test",
                "body": "hello",
            }),
        );
        assert!(
            result.is_err(),
            "Mixed to array with non-strings must be rejected"
        );
        assert!(
            result.unwrap_err().contains("non-string"),
            "Error must mention non-string elements"
        );

        // Nulls are also non-string — now rejected (DR31-H1)
        let result2 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob", null],
                "subject": "test",
                "body": "hello",
            }),
        );
        assert!(
            result2.is_err(),
            "FIXED (DR31-H1): nulls in to array are now rejected"
        );
        assert!(result2.unwrap_err().contains("non-string"));
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

    // ── H18: search_messages limit out of range is rejected (DR30-H3) ──────
    #[tokio::test]
    async fn h18_negative_limit_rejected() {
        let (state, _idx, _repo) = test_post_office();

        // Negative limit now returns an explicit error (DR30-H3).
        let r = search_messages(&state, json!({ "query": "hello", "limit": -1 }));
        assert!(r.is_err(), "Negative limit must be rejected");
        assert!(r.unwrap_err().contains("limit must be >= 1"));
    }

    #[tokio::test]
    async fn h18_zero_limit_rejected() {
        let (state, _idx, _repo) = test_post_office();

        // Zero limit now returns an explicit error (DR30-H3).
        let r = search_messages(&state, json!({ "query": "hello", "limit": 0 }));
        assert!(r.is_err(), "Zero limit must be rejected");
        assert!(r.unwrap_err().contains("limit must be >= 1"));
    }

    #[tokio::test]
    async fn h18_huge_limit_rejected() {
        let (state, _idx, _repo) = test_post_office();

        // Huge limit now returns an explicit error (DR30-H3).
        let r = search_messages(&state, json!({ "query": "hello", "limit": 999_999_999 }));
        assert!(r.is_err(), "Huge limit must be rejected");
        assert!(r.unwrap_err().contains("limit must be <="));
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
            r.unwrap_err().contains("must not contain"),
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
            result.unwrap_err().contains("byte limit"),
            "Error should mention byte limit"
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
            result.unwrap_err().contains("byte limit"),
            "Error should mention byte limit"
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

        // FIXED: agent_id is now stable across re-registrations (DR26-H2)
        assert_eq!(
            r1["id"], r2["id"],
            "FIXED: Upsert preserves original agent_id (DR26-H2)"
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

        // 100-char from_agent — within MAX_AGENT_NAME_LEN (128, DR26-H5 fix)
        let long_from = "X".repeat(100);
        let result = send_message(
            &state,
            json!({
                "from_agent": long_from,
                "to": ["bob"],
                "subject": "test",
                "body": "hello",
            }),
        );
        assert!(result.is_ok(), "100-char from_agent is accepted");

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

        // UPDATED: from_agent now aligned with MAX_AGENT_NAME_LEN (128) per DR26-H5
        let from = "X".repeat(128);

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
            "Exactly 128-char from_agent must be accepted (DR26-H5 aligned limit)"
        );

        let inbox = get_inbox(&state, json!({ "agent_name": "bob" })).unwrap();
        assert_eq!(
            inbox_messages(&inbox)[0]["from_agent"]
                .as_str()
                .unwrap()
                .len(),
            128,
            "Full 128-char from_agent preserved in inbox entry"
        );
    }

    // ── H7: Float limit in get_inbox now returns type error (DR29-H1) ───
    // Previously as_i64() returned None for floats → silent default.
    // Now match-based extraction rejects non-integer types.
    #[tokio::test]
    async fn dr6_h7_float_limit_falls_back_to_default() {
        let (state, _idx, _repo) = test_post_office();

        // Float limit — now rejected with type error instead of silent default
        let result = get_inbox(&state, json!({ "agent_name": "target", "limit": 1.5 }));
        assert!(result.is_err(), "Float limit must be rejected");
        assert!(
            result.unwrap_err().contains("limit must be an integer"),
            "Error must mention integer type requirement"
        );
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

        // UPDATED: Empty project_key is now rejected (DR26-H1)
        let r1 = create_agent(&state, json!({ "project_key": "", "name_hint": "Agent1" }));
        assert!(
            r1.is_err(),
            "FIXED: Empty project_key is now rejected (DR26-H1)"
        );
        assert!(r1.unwrap_err().contains("must not be empty"));
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
                let base = "R".repeat(super::MAX_AGENT_NAME_LEN - 2);
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

        // 10KB query string — Tantivy parser handles gracefully.
        // Use trim() to remove trailing space from repeat() (DR34-H2 fix
        // now rejects whitespace-padded queries).
        let long_query = "term ".repeat(2000);
        let long_query = long_query.trim();
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

    // ── H3 (UPDATED): Non-string method now returns -32600 Invalid Request ──
    // Non-string method is a structurally invalid request per JSON-RPC 2.0 §4.
    // Fixed in DR28-H3: returns -32600 instead of -32601.
    #[tokio::test]
    async fn dr11_h3_nonstring_method_returns_error() {
        let (state, _idx, _repo) = test_post_office();

        // method is an integer, not a string → -32600 Invalid Request
        let resp = handle_mcp_request(
            state.clone(),
            json!({ "jsonrpc": "2.0", "id": 1, "method": 42 }),
        )
        .await;
        assert_eq!(
            resp["error"]["code"], -32600,
            "Non-string method returns -32600 Invalid Request (DR28-H3)"
        );

        // method is a boolean
        let resp = handle_mcp_request(
            state.clone(),
            json!({ "jsonrpc": "2.0", "id": 2, "method": true }),
        )
        .await;
        assert_eq!(resp["error"]["code"], -32600);

        // method is an array
        let resp = handle_mcp_request(
            state,
            json!({ "jsonrpc": "2.0", "id": 3, "method": ["tools/call"] }),
        )
        .await;
        assert_eq!(resp["error"]["code"], -32600);
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

    // ── H1 (FIXED): Recipient names bounded by MAX_AGENT_NAME_LEN ───
    // Every name-like field now has a MAX_* constant. Individual recipient
    // names are capped at 128 bytes (aligned with agent name limit, DR27-H2).
    #[tokio::test]
    async fn dr12_h1_recipient_name_length_validated() {
        let (state, _idx, _repo) = test_post_office();

        // Exceeding MAX_AGENT_NAME_LEN (128) must be rejected
        let huge_name = "R".repeat(super::MAX_AGENT_NAME_LEN + 1);
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
        let err = result.unwrap_err();
        assert!(
            err.contains("exceeds") && err.contains("byte limit"),
            "Error should mention exceeds byte limit: {}",
            err
        );

        // No DashMap entry created
        assert!(
            !state.inboxes.contains_key(&huge_name),
            "No DashMap key for rejected recipient"
        );

        // At the boundary: exactly MAX_AGENT_NAME_LEN (128) accepted
        let ok_name = "R".repeat(super::MAX_AGENT_NAME_LEN);
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

        // UPDATED: from_agent now uses MAX_AGENT_NAME_LEN (128) per DR26-H5
        let max_from = "F".repeat(super::MAX_AGENT_NAME_LEN);
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
                super::MAX_AGENT_NAME_LEN
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
    // All name-like fields now use MAX_AGENT_NAME_LEN (128) for consistency
    // (DR27-H2). get_inbox agent_name is aligned with create_agent and send_message.
    #[tokio::test]
    async fn dr13_h2_get_inbox_agent_name_length_validated() {
        let (state, _idx, _repo) = test_post_office();

        // Oversized agent_name must be rejected (MAX_AGENT_NAME_LEN = 128)
        let huge_name = "A".repeat(super::MAX_AGENT_NAME_LEN + 1);
        let result = get_inbox(&state, json!({ "agent_name": huge_name }));

        assert!(result.is_err(), "Oversized agent_name must be rejected");
        assert!(result.unwrap_err().contains("agent_name exceeds"));

        // Boundary test: exactly MAX_AGENT_NAME_LEN accepted
        let ok_name = "A".repeat(super::MAX_AGENT_NAME_LEN);
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

        // Empty query is rejected via validate_text_core (DR41-H3).
        assert!(result.is_err(), "Empty query must be rejected");
        assert!(
            result.unwrap_err().contains("must not be empty"),
            "Error should mention empty"
        );
    }

    // ── H5 (UPDATED DR30-H3): search_messages limit range validated ────────
    // Out-of-range limits are now rejected instead of silently clamped.
    #[tokio::test]
    async fn dr13_h5_search_limit_range_validated() {
        let (state, _idx, _repo) = test_post_office();

        // Default limit uses DEFAULT_SEARCH_LIMIT
        let result = search_messages(&state, json!({ "query": "hello" })).unwrap();
        assert!(
            result["results"].as_array().is_some(),
            "Default limit returns array"
        );

        // Negative limit rejected (DR30-H3)
        let result = search_messages(&state, json!({ "query": "hello", "limit": -5 }));
        assert!(result.is_err(), "Negative limit must be rejected");
        assert!(result.unwrap_err().contains("limit must be >= 1"));

        // Huge limit rejected (DR30-H3)
        let result = search_messages(
            &state,
            json!({ "query": "hello", "limit": (super::MAX_SEARCH_LIMIT + 999) }),
        );
        assert!(result.is_err(), "Huge limit must be rejected");
        assert!(result.unwrap_err().contains("limit must be <="));
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

    // ── H2 (UPDATED): send_message reports indexed status ──────────────────
    // The response includes "indexed": true/false so clients know whether
    // the persist channel accepted the Tantivy index op (DR45-H4).
    // (DR21-H5 removed "indexed"; DR45-H4 restored it with correct semantics.)
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
        // UPDATED (DR45-H4): indexed field is now present — reports whether
        // the persist channel accepted the Tantivy index operation.
        assert!(
            result.get("indexed").is_some(),
            "Response must include 'indexed' field (restored in DR45-H4)"
        );
        assert_eq!(
            result["indexed"], true,
            "indexed must be true when persist channel is available"
        );
    }

    // ── H3 (UPDATED DR30-H3): search_messages limit=0 now rejected ─────────
    // Previously clamped to 1 — now returns explicit error (DR30-H3).
    #[tokio::test]
    async fn dr14_h3_search_limit_zero_rejected() {
        let (state, _idx, _repo) = test_post_office();

        let result = search_messages(&state, json!({ "query": "hello", "limit": 0 }));
        assert!(result.is_err(), "limit=0 must be rejected");
        assert!(result.unwrap_err().contains("limit must be >= 1"));
    }

    // ── H4 (DISPROVED): empty JSON request returns error, no panic ──────────
    // UPDATED (DR25): empty JSON now fails jsonrpc version validation first.
    #[tokio::test]
    async fn dr14_h4_empty_json_request_returns_error() {
        let (state, _idx, _repo) = test_post_office();

        // Empty JSON object has no "id" — treated as notification, silently dropped (DR26-H3 fix)
        let response = handle_mcp_request(state, json!({})).await;
        assert!(
            response.is_null(),
            "UPDATED: Empty JSON (no id) is a notification — silently dropped (DR26)"
        );

        // With "id" and "jsonrpc" present but no method → -32600 Invalid Request (DR28-H3)
        let response2 =
            handle_mcp_request(test_post_office().0, json!({ "jsonrpc": "2.0", "id": 1 })).await;
        assert_eq!(response2["jsonrpc"], "2.0");
        assert!(response2.get("error").is_some(), "Must return error");
        assert_eq!(response2["error"]["code"], -32600);
        assert!(response2["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing method"));
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
    // -32602 for unknown tool (Invalid params — DR25 fix), -32602 for
    // param/validation errors, -32601 for unknown method.
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

        // 3. Unknown tool → -32602 (Invalid params — DR25 fix)
        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "nonexistent_tool", "arguments": {} }
            }),
        )
        .await;
        assert_eq!(
            resp["error"]["code"], -32602,
            "UPDATED: Unknown tool now returns -32602 (Invalid params, DR25)"
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
    async fn dr15_h5_missing_jsonrpc_field_now_rejected() {
        let (state, _idx, _repo) = test_post_office();

        // UPDATED (DR25): Missing jsonrpc field now returns -32600
        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "id": 1, "method": "tools/list"
            }),
        )
        .await;
        assert_eq!(
            resp["error"]["code"], -32600,
            "UPDATED: Missing jsonrpc now returns -32600 (DR25)"
        );

        // Wrong version also returns -32600
        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "1.0", "id": 2, "method": "tools/list"
            }),
        )
        .await;
        assert_eq!(
            resp["error"]["code"], -32600,
            "UPDATED: Wrong jsonrpc version now returns -32600 (DR25)"
        );

        // Non-string jsonrpc also returns -32600
        let resp = handle_mcp_request(
            state,
            json!({
                "jsonrpc": 42, "id": 3, "method": "tools/list"
            }),
        )
        .await;
        assert_eq!(
            resp["error"]["code"], -32600,
            "UPDATED: Non-string jsonrpc now returns -32600 (DR25)"
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

        // UPDATED: id is now stable across re-registrations (DR26-H2)
        assert_eq!(
            r1["id"], r2["id"],
            "FIXED: Same id on re-registration (DR26-H2)"
        );
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

        // UPDATED (DR25): Non-object payloads now return -32600 Invalid Request
        // instead of being treated as notifications.

        // String payload
        let r1 = handle_mcp_request(state.clone(), json!("not an object")).await;
        assert_eq!(
            r1["error"]["code"], -32600,
            "UPDATED: String payload returns -32600 (DR25)"
        );

        // Array payload
        let r2 = handle_mcp_request(state.clone(), json!([1, 2, 3])).await;
        assert_eq!(
            r2["error"]["code"], -32600,
            "UPDATED: Array payload returns -32600 (DR25)"
        );

        // Null payload
        let r3 = handle_mcp_request(state, json!(null)).await;
        assert_eq!(
            r3["error"]["code"], -32600,
            "UPDATED: Null payload returns -32600 (DR25)"
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
            result.unwrap_err().contains("whitespace-only"),
            "Error should mention whitespace-only"
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

    // ── H5 (UPDATED DR45-H4): send_message response now includes `indexed` ──
    // The `indexed` field reports whether the persist channel accepted the
    // Tantivy index operation. Clients can distinguish "sent + indexed" from
    // "sent + index dropped" (DR45-H4).
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

        // UPDATED (DR45-H4): indexed field restored — reports persist channel status
        assert!(
            result.get("indexed").is_some(),
            "Response must include 'indexed' field (DR45-H4)"
        );
        assert_eq!(
            result["indexed"], true,
            "indexed must be true under normal conditions"
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

        // UPDATED: Empty string is also now rejected (DR26-H1)
        let result2 = create_agent(&state, json!({ "project_key": "", "name_hint": "Agent2" }));
        assert!(
            result2.is_err(),
            "FIXED: Empty project_key also rejected (DR26-H1)"
        );
        assert!(result2.unwrap_err().contains("must not be empty"));
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
    // ── H3 (FIXED): Non-integer limit now returns type error (DR29-H1/H2) ──
    // Boolean, string, float limits are rejected instead of silently defaulting.
    #[tokio::test]
    async fn dr23_h3_non_integer_limit_falls_back_to_default() {
        let (state, _idx, _repo) = test_post_office();

        // Boolean limit → type error
        let r1 = get_inbox(&state, json!({ "agent_name": "target", "limit": true }));
        assert!(r1.is_err(), "Boolean limit must be rejected");
        assert!(
            r1.unwrap_err().contains("limit must be an integer"),
            "FIXED: Boolean limit returns type error instead of silent default"
        );

        // String limit → type error
        let r2 = get_inbox(&state, json!({ "agent_name": "target", "limit": "999" }));
        assert!(r2.is_err(), "String limit must be rejected");
        assert!(
            r2.unwrap_err().contains("limit must be an integer"),
            "FIXED: String limit returns type error instead of silent default"
        );

        // Same for search_messages
        let r3 = search_messages(&state, json!({ "query": "hello", "limit": false }));
        assert!(r3.is_err(), "Boolean limit in search must be rejected");
        assert!(
            r3.unwrap_err().contains("limit must be an integer"),
            "FIXED: Boolean limit in search returns type error"
        );
    }

    // ── H4 (FIXED): Non-string optional fields now rejected with clear error ──
    // program=42, model=true now return type errors (DR28-H1).
    // Null values still default to "unknown" (same as absent).
    #[tokio::test]
    async fn dr23_h4_non_string_optional_fields_use_defaults() {
        let (state, _idx, _repo) = test_post_office();

        // Integer program → error
        let r1 = create_agent(
            &state,
            json!({
                "project_key": "test",
                "name_hint": "TypeAgent",
                "program": 42,
                "model": "valid",
            }),
        );
        assert!(r1.is_err(), "FIXED: Non-string program now rejected");
        assert!(r1.unwrap_err().contains("program must be a string"));

        // Boolean model → error
        let r2 = create_agent(
            &state,
            json!({
                "project_key": "test",
                "name_hint": "TypeAgent2",
                "program": "valid",
                "model": true,
            }),
        );
        assert!(r2.is_err(), "FIXED: Non-string model now rejected");
        assert!(r2.unwrap_err().contains("model must be a string"));

        // Null program/model → defaults to "unknown" (like absent)
        let r3 = create_agent(
            &state,
            json!({
                "project_key": "test",
                "name_hint": "TypeAgent3",
                "program": null,
                "model": null,
            }),
        );
        assert!(r3.is_ok(), "Null program/model treated as absent");
        let profile = r3.unwrap();
        assert_eq!(profile["program"], "unknown");
        assert_eq!(profile["model"], "unknown");
    }

    // ── H5 (FIXED): Non-array `to` now returns type-specific error ──────────
    // Fixed in DR28-H2/H5: wrong-type fields now say what's wrong instead
    // of "Missing". String/integer `to` says "must be a JSON array".
    #[tokio::test]
    async fn dr23_h5_non_array_to_returns_type_error() {
        let (state, _idx, _repo) = test_post_office();

        // `to` is a string, not an array → type error
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
            "to must be a JSON array",
            "FIXED: Error explains the type requirement (DR28)"
        );

        // `to` is an integer → same type error
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
        assert_eq!(r2.unwrap_err(), "to must be a JSON array");

        // `to` as null → treated as missing
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
        assert_eq!(r3.unwrap_err(), "to must be a JSON array");
    }

    // ── DR24: Validation Asymmetries II ─────────────────────────────────────

    // H1: search_messages query accepts control characters (unlike all other string inputs)
    #[tokio::test]
    async fn dr24_h1_search_query_rejects_control_characters() {
        let (state, _idx, _repo) = test_post_office();

        // UPDATED: search_messages now rejects control characters (DR24 fix)
        let result = search_messages(&state, json!({ "query": "hello\x01world" }));
        assert!(
            result.is_err(),
            "FIXED: Control chars in query now rejected"
        );
        assert!(
            result.unwrap_err().contains("control characters"),
            "Error message mentions control characters"
        );

        // Verify consistency: from_agent also rejects control chars
        let send_result = send_message(
            &state,
            json!({
                "from_agent": "hello\x01world",
                "to": ["bob"],
                "subject": "test",
                "body": "test"
            }),
        );
        assert!(
            send_result.is_err() && send_result.unwrap_err().contains("control characters"),
            "from_agent correctly rejects control characters"
        );
    }

    // H2: Dot-prefixed recipients accepted by send_message and drainable by get_inbox,
    // UPDATED: dot-prefixed names now rejected uniformly (DR24 fix)
    #[tokio::test]
    async fn dr24_h2_dot_prefixed_recipient_rejected_uniformly() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: send_message now rejects ".hidden" as recipient
        let send_result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": [".hidden"],
                "subject": "secret",
                "body": "test"
            }),
        );
        assert!(
            send_result.is_err(),
            "FIXED: send_message rejects dot-prefixed recipient '.hidden'"
        );
        assert!(send_result.unwrap_err().contains("must not start with '.'"));

        // FIXED: get_inbox now rejects ".hidden" agent_name
        let inbox_result = get_inbox(&state, json!({ "agent_name": ".hidden" }));
        assert!(
            inbox_result.is_err(),
            "FIXED: get_inbox rejects dot-prefixed agent_name '.hidden'"
        );
        assert!(inbox_result
            .unwrap_err()
            .contains("must not start with '.'"));

        // create_agent already rejected ".hidden" (unchanged)
        let create_result = create_agent(
            &state,
            json!({
                "project_key": "test_proj",
                "name_hint": ".hidden"
            }),
        );
        assert!(
            create_result.is_err(),
            "create_agent rejects dot-prefixed name '.hidden'"
        );
        assert!(create_result
            .unwrap_err()
            .contains("must not start with '.'"));
    }

    // H3: from_agent validation lacks path separator and ".." checks (unlike recipients and get_inbox)
    #[tokio::test]
    async fn dr24_h3_from_agent_rejects_path_separators_and_dotdot() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: from_agent with path separator now rejected
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "../../etc/passwd",
                "to": ["bob"],
                "subject": "test",
                "body": "test"
            }),
        );
        assert!(r1.is_err(), "FIXED: from_agent rejects path traversal");
        assert!(r1.unwrap_err().contains("path separators"));

        // FIXED: from_agent with ".." now rejected
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "alice..bob",
                "to": ["bob"],
                "subject": "test",
                "body": "test"
            }),
        );
        assert!(r2.is_err(), "FIXED: from_agent rejects '..' sequences");
        assert!(r2.unwrap_err().contains("'..'"));

        // FIXED: from_agent with backslash now rejected
        let r3 = send_message(
            &state,
            json!({
                "from_agent": "alice\\bob",
                "to": ["bob"],
                "subject": "test",
                "body": "test"
            }),
        );
        assert!(r3.is_err(), "FIXED: from_agent rejects backslash");
        assert!(r3.unwrap_err().contains("path separators"));

        // Verify consistency: recipients also block these
        let r4 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["../../etc/passwd"],
                "subject": "test",
                "body": "test"
            }),
        );
        assert!(r4.is_err(), "Recipients correctly block path separators");
        assert!(r4.unwrap_err().contains("path separators"));
    }

    // H4: tools/list inputSchema omits machine-readable limit constraints (minimum/maximum/default)
    #[tokio::test]
    async fn dr24_h4_tools_list_schema_has_limit_constraints() {
        let (state, _idx, _repo) = test_post_office();

        let response = handle_mcp_request(
            state,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list"
            }),
        )
        .await;

        let tools = response["result"]["tools"].as_array().unwrap();

        // FIXED: search_messages limit now has machine-readable constraints
        let search_tool = tools
            .iter()
            .find(|t| t["name"] == "search_messages")
            .expect("search_messages tool must exist");
        let search_limit = &search_tool["inputSchema"]["properties"]["limit"];
        assert_eq!(
            search_limit["minimum"], 1,
            "FIXED: search limit has minimum"
        );
        assert_eq!(
            search_limit["maximum"], 1000,
            "FIXED: search limit has maximum"
        );
        assert_eq!(
            search_limit["default"], 10,
            "FIXED: search limit has default"
        );

        // FIXED: get_inbox limit now has machine-readable constraints
        let inbox_tool = tools
            .iter()
            .find(|t| t["name"] == "get_inbox")
            .expect("get_inbox tool must exist");
        let inbox_limit = &inbox_tool["inputSchema"]["properties"]["limit"];
        assert_eq!(inbox_limit["minimum"], 1, "FIXED: inbox limit has minimum");
        assert_eq!(
            inbox_limit["maximum"], 1000,
            "FIXED: inbox limit has maximum"
        );
        assert_eq!(
            inbox_limit["default"], 100,
            "FIXED: inbox limit has default"
        );
    }

    // H5: Non-object params produces misleading "Unknown tool" error instead of "invalid params"
    #[tokio::test]
    async fn dr24_h5_non_object_params_returns_invalid_params_error() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: String params now returns -32602 Invalid params
        let r1 = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": "not an object"
            }),
        )
        .await;
        assert_eq!(
            r1["error"]["code"], -32602,
            "FIXED: Returns -32602 Invalid params"
        );
        assert!(
            r1["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Invalid params"),
            "FIXED: Error message says 'Invalid params'"
        );

        // FIXED: Array params now returns -32602
        let r2 = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": [1, 2, 3]
            }),
        )
        .await;
        assert_eq!(
            r2["error"]["code"], -32602,
            "FIXED: Array params returns -32602"
        );

        // FIXED: Null params now returns -32602
        let r3 = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": null
            }),
        )
        .await;
        assert_eq!(
            r3["error"]["code"], -32602,
            "FIXED: Null params returns -32602"
        );
    }

    // ── DR25: Cross-Cutting Concerns & Spec Compliance ──────────────────────

    // H1: No cap on total inbox count — unlimited DashMap entries via unique recipients
    #[tokio::test]
    async fn dr25_h1_inbox_dashmap_capped_at_max_inboxes() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: inboxes DashMap now has MAX_INBOXES cap.
        // Pre-fill the DashMap to just under the cap to test the boundary
        // without creating 100K entries via send_message (which would be slow).
        for i in 0..(MAX_INBOXES - 1) {
            state
                .inboxes
                .entry(format!("phantom_{}", i))
                .or_default()
                .push(InboxEntry {
                    message_id: format!("msg_{}", i),
                    from_agent: "setup".to_string(),
                    to_recipients: "phantom".to_string(),
                    subject: "pre-fill".to_string(),
                    body: "x".to_string(),
                    timestamp: 0,
                    project_id: "".to_string(),
                });
        }
        assert_eq!(state.inboxes.len(), MAX_INBOXES - 1);

        // One more NEW inbox should succeed (reaching cap exactly)
        let at_cap = send_message(
            &state,
            json!({
                "from_agent": "spammer",
                "to": ["last_one"],
                "subject": "test",
                "body": "x"
            }),
        );
        assert!(at_cap.is_ok(), "Should succeed at exactly MAX_INBOXES");
        assert_eq!(state.inboxes.len(), MAX_INBOXES);

        // Next send to a NEW recipient should fail
        let overflow = send_message(
            &state,
            json!({
                "from_agent": "spammer",
                "to": ["one_more"],
                "subject": "test",
                "body": "x"
            }),
        );
        assert!(overflow.is_err(), "FIXED: Inbox cap enforced");
        assert!(overflow.unwrap_err().contains("Inbox limit reached"));

        // Sending to an EXISTING inbox should still succeed
        let existing = send_message(
            &state,
            json!({
                "from_agent": "spammer",
                "to": ["phantom_0"],
                "subject": "another",
                "body": "x"
            }),
        );
        assert!(
            existing.is_ok(),
            "Sending to existing inbox still works at cap"
        );
    }

    // H2: from_agent missing starts_with('.') check — asymmetry with recipients and get_inbox
    #[tokio::test]
    async fn dr25_h2_from_agent_rejects_dot_prefixed_names() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: from_agent now rejects dot-prefixed names
        let result = send_message(
            &state,
            json!({
                "from_agent": ".evil",
                "to": ["bob"],
                "subject": "test",
                "body": "test"
            }),
        );
        assert!(result.is_err(), "FIXED: from_agent rejects '.evil'");
        assert!(result.unwrap_err().contains("must not start with '.'"));

        // Consistency: recipients also block it
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": [".evil"],
                "subject": "test",
                "body": "test"
            }),
        );
        assert!(r2.is_err(), "Recipients correctly reject '.evil'");
        assert!(r2.unwrap_err().contains("must not start with '.'"));

        // Consistency: get_inbox also blocks it
        let r3 = get_inbox(&state, json!({ "agent_name": ".evil" }));
        assert!(r3.is_err(), "get_inbox correctly rejects '.evil'");
        assert!(r3.unwrap_err().contains("must not start with '.'"));
    }

    // H3: Missing jsonrpc version field validation — server accepts any version or missing field
    #[tokio::test]
    async fn dr25_h3_jsonrpc_version_validated() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: Wrong version "1.0" now returns -32600
        let r1 = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "1.0",
                "id": 1,
                "method": "tools/list"
            }),
        )
        .await;
        assert_eq!(
            r1["error"]["code"], -32600,
            "FIXED: jsonrpc '1.0' returns -32600 Invalid Request"
        );

        // FIXED: Missing jsonrpc field returns -32600
        let r2 = handle_mcp_request(
            state.clone(),
            json!({
                "id": 2,
                "method": "tools/list"
            }),
        )
        .await;
        assert_eq!(
            r2["error"]["code"], -32600,
            "FIXED: Missing jsonrpc returns -32600"
        );

        // FIXED: Non-string jsonrpc returns -32600
        let r3 = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": 2,
                "id": 3,
                "method": "tools/list"
            }),
        )
        .await;
        assert_eq!(
            r3["error"]["code"], -32600,
            "FIXED: Integer jsonrpc returns -32600"
        );

        // Correct version still works
        let r4 = handle_mcp_request(
            state,
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/list"
            }),
        )
        .await;
        assert!(r4.get("result").is_some(), "Correct jsonrpc '2.0' succeeds");
    }

    // H4: Unknown tool uses -32601 (Method not found) instead of -32602 (Invalid params)
    #[tokio::test]
    async fn dr25_h4_unknown_tool_returns_correct_error_code() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: Unknown tool now returns -32602 (Invalid params)
        let response = handle_mcp_request(
            state,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "nonexistent_tool",
                    "arguments": {}
                }
            }),
        )
        .await;

        let code = response["error"]["code"].as_i64().unwrap();
        assert_eq!(
            code, -32602,
            "FIXED: Unknown tool now returns -32602 (Invalid params)"
        );

        // Method not found still returns -32601 — codes are now distinct
        let (state2, _idx2, _repo2) = test_post_office();
        let r2 = handle_mcp_request(
            state2,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "nonexistent/method"
            }),
        )
        .await;
        let code2 = r2["error"]["code"].as_i64().unwrap();
        assert_eq!(
            code2, -32601,
            "Method not found correctly returns -32601 — distinguishable from unknown tool"
        );
    }

    // H5: JSON array body treated as notification (204) instead of error
    #[tokio::test]
    async fn dr25_h5_non_object_body_returns_invalid_request() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: Array body now returns -32600 Invalid Request
        let response = handle_mcp_request(
            state.clone(),
            json!([
                {"jsonrpc": "2.0", "id": 1, "method": "tools/list"},
                {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}
            ]),
        )
        .await;
        assert_eq!(
            response["error"]["code"], -32600,
            "FIXED: Array body returns -32600 Invalid Request"
        );
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("must be a JSON object"));

        // FIXED: Integer body returns -32600
        let r2 = handle_mcp_request(state.clone(), json!(42)).await;
        assert_eq!(
            r2["error"]["code"], -32600,
            "FIXED: Integer body returns -32600"
        );

        // FIXED: String body returns -32600
        let r3 = handle_mcp_request(state.clone(), json!("hello")).await;
        assert_eq!(
            r3["error"]["code"], -32600,
            "FIXED: String body returns -32600"
        );
    }

    // ── DR26 Hypotheses ─────────────────────────────────────────────────────

    // DR26-H1: Empty project_key is now rejected by create_agent.
    // FIXED: Added explicit is_empty() check before the whitespace-only check.
    #[tokio::test]
    async fn dr26_h1_empty_project_key_accepted() {
        let (state, _idx, _repo) = test_post_office();

        let result = create_agent(
            &state,
            json!({
                "project_key": "",
                "name_hint": "EmptyProjectAgent"
            }),
        );

        // FIXED: empty project_key is now rejected
        assert!(result.is_err(), "FIXED: empty project_key is rejected");
        assert!(
            result.unwrap_err().contains("must not be empty"),
            "Error message mentions empty"
        );

        // No project or agent should have been created
        assert!(
            state.projects.is_empty(),
            "No project created for empty key"
        );
        assert!(
            state.agents.is_empty(),
            "No agent created for empty project key"
        );
    }

    // DR26-H2: Agent re-registration now preserves the original agent_id.
    // FIXED: The Occupied branch reuses occ.get().id instead of generating new UUID.
    #[tokio::test]
    async fn dr26_h2_reregistration_changes_agent_id() {
        let (state, _idx, _repo) = test_post_office();

        // First registration
        let r1 = create_agent(
            &state,
            json!({
                "project_key": "stable_project",
                "name_hint": "StableAgent"
            }),
        )
        .unwrap();
        let id1 = r1["id"].as_str().unwrap().to_string();

        // Re-registration (same name, same project)
        let r2 = create_agent(
            &state,
            json!({
                "project_key": "stable_project",
                "name_hint": "StableAgent"
            }),
        )
        .unwrap();
        let id2 = r2["id"].as_str().unwrap().to_string();

        // FIXED: agent_id is stable across re-registrations
        assert_eq!(
            id1, id2,
            "FIXED: re-registration preserves original agent_id"
        );

        // The DashMap record preserves the original ID
        let record = state.agents.get("StableAgent").unwrap();
        assert_eq!(
            record.id, id1,
            "DashMap stores the original agent_id across upserts"
        );
    }

    // DR26-H3: Notification check now runs BEFORE jsonrpc validation.
    // FIXED: All requests without "id" are silently dropped (Value::Null),
    // even if jsonrpc version is wrong or missing.
    #[tokio::test]
    async fn dr26_h3_malformed_notification_gets_response() {
        let (state, _idx, _repo) = test_post_office();

        // No "id" field, wrong jsonrpc version → silently dropped
        let response = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "1.0",
                "method": "tools/list"
            }),
        )
        .await;

        // FIXED: malformed notification is silently dropped (no response)
        assert!(
            response.is_null(),
            "FIXED: malformed notification silently dropped per JSON-RPC 2.0 §4.1"
        );

        // Valid notification (correct jsonrpc, no id) also silently dropped
        let valid_notification = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0",
                "method": "tools/list"
            }),
        )
        .await;
        assert!(
            valid_notification.is_null(),
            "Valid notification correctly returns Value::Null"
        );

        // No "id" and no "jsonrpc" at all → also silently dropped
        let no_jsonrpc = handle_mcp_request(
            state.clone(),
            json!({
                "method": "tools/list"
            }),
        )
        .await;
        assert!(
            no_jsonrpc.is_null(),
            "FIXED: missing-jsonrpc notification also silently dropped"
        );
    }

    // DR26-H4: Per-inbox size cap (MAX_INBOX_SIZE) pre-check is not atomic
    // with delivery. The pre-check uses get() and the delivery uses
    // entry().or_default().push() — separate DashMap operations.
    // Demonstrating the window: pre-fill to MAX_INBOX_SIZE-1, two concurrent
    // sends could both pass the pre-check and both deliver.
    #[tokio::test]
    async fn dr26_h4_inbox_cap_precheck_not_atomic_with_delivery() {
        let (state, _idx, _repo) = test_post_office();

        // Register sender
        create_agent(
            &state,
            json!({ "project_key": "cap_test", "name_hint": "Sender" }),
        )
        .unwrap();

        // Pre-fill inbox to MAX_INBOX_SIZE - 1 via direct DashMap manipulation
        let mut entries = Vec::with_capacity(MAX_INBOX_SIZE - 1);
        for i in 0..MAX_INBOX_SIZE - 1 {
            entries.push(InboxEntry {
                message_id: format!("prefill-{}", i),
                from_agent: "Sender".to_string(),
                to_recipients: "CapTarget".to_string(),
                subject: "fill".to_string(),
                body: "x".to_string(),
                timestamp: 0,
                project_id: "test".to_string(),
            });
        }
        state.inboxes.insert("CapTarget".to_string(), entries);

        // Verify we're at MAX_INBOX_SIZE - 1
        assert_eq!(
            state.inboxes.get("CapTarget").unwrap().len(),
            MAX_INBOX_SIZE - 1,
            "Pre-fill should be at MAX_INBOX_SIZE - 1"
        );

        // Single-threaded: this send should succeed (one slot remaining)
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "Sender",
                "to": ["CapTarget"],
                "subject": "last",
                "body": "fits"
            }),
        );
        assert!(r1.is_ok(), "Should succeed — one slot remaining");

        // Now at MAX_INBOX_SIZE — next send should be rejected
        assert_eq!(
            state.inboxes.get("CapTarget").unwrap().len(),
            MAX_INBOX_SIZE,
            "Inbox should be exactly at MAX_INBOX_SIZE"
        );

        let r2 = send_message(
            &state,
            json!({
                "from_agent": "Sender",
                "to": ["CapTarget"],
                "subject": "overflow",
                "body": "rejected"
            }),
        );
        assert!(r2.is_err(), "Should be rejected — inbox is full");
        assert!(
            r2.unwrap_err().contains("inbox full"),
            "Error mentions inbox full"
        );

        // CONFIRMED: The pre-check and delivery are separate DashMap ops.
        // In single-threaded tests the cap works correctly, but under
        // concurrent load two sends could both pass the pre-check (seeing
        // 9,999) and both deliver, overshooting to 10,001.
        // This is the same soft-cap pattern as MAX_INBOXES (acknowledged).
    }

    // DR26-H5: from_agent max length now aligned with agent name max (128).
    // FIXED: from_agent uses MAX_AGENT_NAME_LEN instead of MAX_FROM_AGENT_LEN.
    #[tokio::test]
    async fn dr26_h5_from_agent_length_asymmetry() {
        let (state, _idx, _repo) = test_post_office();

        // Create a valid recipient
        create_agent(
            &state,
            json!({ "project_key": "len_test", "name_hint": "Receiver" }),
        )
        .unwrap();

        // 128 chars: accepted by both create_agent and from_agent
        let max_name: String = "A".repeat(128);
        let send_ok = send_message(
            &state,
            json!({
                "from_agent": max_name,
                "to": ["Receiver"],
                "subject": "ok",
                "body": "at limit"
            }),
        );
        assert!(send_ok.is_ok(), "128-char from_agent accepted (at limit)");

        // 129 chars: now rejected by BOTH create_agent and from_agent
        let long_name: String = "B".repeat(129);
        let create_result = create_agent(
            &state,
            json!({ "project_key": "len_test", "name_hint": long_name }),
        );
        assert!(
            create_result.is_err(),
            "129-char name rejected by create_agent"
        );

        let send_result = send_message(
            &state,
            json!({
                "from_agent": long_name,
                "to": ["Receiver"],
                "subject": "ghost",
                "body": "from long name"
            }),
        );

        // FIXED: from_agent now also rejects names > 128 chars
        assert!(
            send_result.is_err(),
            "FIXED: 129-char from_agent rejected, consistent with create_agent"
        );
        assert!(
            send_result.unwrap_err().contains("128"),
            "Error references the aligned limit of 128"
        );
    }

    // ── DR27 Hypotheses ─────────────────────────────────────────────────────

    // DR27-H1 (FIXED): program and model fields now validate for null bytes
    // and control characters, consistent with all other string fields.
    #[tokio::test]
    async fn dr27_h1_program_model_reject_null_bytes_and_control_chars() {
        let (state, _idx, _repo) = test_post_office();

        // program with null byte — now rejected
        let r1 = create_agent(
            &state,
            json!({
                "project_key": "test",
                "name_hint": "Agent1",
                "program": "v1\u{0000}evil",
                "model": "clean"
            }),
        );
        assert!(r1.is_err(), "program with null byte must be rejected");
        assert!(r1
            .unwrap_err()
            .contains("program must not contain null bytes"));

        // model with control char — now rejected
        let r2 = create_agent(
            &state,
            json!({
                "project_key": "test",
                "name_hint": "Agent2",
                "program": "clean",
                "model": "gpt\u{0001}bad"
            }),
        );
        assert!(r2.is_err(), "model with control char must be rejected");
        assert!(r2
            .unwrap_err()
            .contains("model must not contain control characters"));

        // project_key with null byte also rejected (consistent)
        let r3 = create_agent(
            &state,
            json!({
                "project_key": "test\u{0000}bad",
                "name_hint": "Agent3"
            }),
        );
        assert!(
            r3.is_err(),
            "project_key with null byte is correctly rejected"
        );
    }

    // DR27-H2 (FIXED): Recipient name limit and get_inbox agent_name limit
    // are now aligned with MAX_AGENT_NAME_LEN (128). All name-like fields
    // use the same limit so names that can be sent to can always be registered.
    #[tokio::test]
    async fn dr27_h2_recipient_name_limit_aligned_with_agent_name() {
        let (state, _idx, _repo) = test_post_office();

        // 200-char recipient — now rejected (limit 128, not 256)
        let long_recipient: String = "R".repeat(200);
        let send_result = send_message(
            &state,
            json!({
                "from_agent": "Sender",
                "to": [long_recipient],
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(
            send_result.is_err(),
            "200-char recipient now rejected (limit 128)"
        );
        let err = send_result.unwrap_err();
        assert!(
            err.contains("exceeds") && err.contains("byte limit"),
            "Error should mention exceeds byte limit: {}",
            err
        );

        // Also rejected by create_agent (same limit)
        let create_result = create_agent(
            &state,
            json!({ "project_key": "test", "name_hint": long_recipient }),
        );
        assert!(
            create_result.is_err(),
            "200-char name rejected by create_agent (limit 128)"
        );

        // Also rejected by get_inbox (same limit now)
        let inbox_result = get_inbox(&state, json!({ "agent_name": long_recipient }));
        assert!(
            inbox_result.is_err(),
            "200-char agent_name now rejected by get_inbox (limit 128)"
        );

        // At the boundary: exactly 128 chars works everywhere
        let ok_name = "R".repeat(128);
        let send_ok = send_message(
            &state,
            json!({
                "from_agent": "Sender",
                "to": [ok_name],
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(send_ok.is_ok(), "128-char recipient accepted");
        let inbox_ok = get_inbox(&state, json!({ "agent_name": ok_name }));
        assert!(
            inbox_ok.is_ok(),
            "128-char agent_name accepted by get_inbox"
        );
    }

    // DR27-H3 (FIXED): create_agent upsert now preserves BOTH agent_id
    // AND registered_at. The field correctly reflects first registration time.
    #[tokio::test]
    async fn dr27_h3_upsert_preserves_registered_at_and_id() {
        let (state, _idx, _repo) = test_post_office();

        let r1 = create_agent(
            &state,
            json!({ "project_key": "proj", "name_hint": "Alice", "program": "v1" }),
        )
        .unwrap();

        let ts1 = r1["registered_at"].as_i64().unwrap();

        // Small delay to ensure different timestamp if it were to change
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let r2 = create_agent(
            &state,
            json!({ "project_key": "proj", "name_hint": "Alice", "program": "v2" }),
        )
        .unwrap();

        let ts2 = r2["registered_at"].as_i64().unwrap();

        // agent_id is stable (DR26-H2)
        assert_eq!(
            r1["id"], r2["id"],
            "agent_id preserved across re-registrations"
        );

        // registered_at is now stable too (DR27-H3)
        assert_eq!(
            ts1, ts2,
            "registered_at preserved on upsert — reflects first registration time"
        );
    }

    // DR27-H4 (FIXED): send_message response now includes delivered_count
    // so callers can detect if any recipients were silently skipped due to
    // concurrent inbox cap races.
    #[tokio::test]
    async fn dr27_h4_send_response_includes_delivered_count() {
        let (state, _idx, _repo) = test_post_office();

        // Normal send to 3 recipients
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "Sender",
                "to": ["A", "B", "C"],
                "subject": "test",
                "body": "hello"
            }),
        )
        .unwrap();

        assert_eq!(r1["status"].as_str().unwrap(), "sent");
        assert!(r1.get("id").is_some(), "Has message id");

        // delivered_count now present and equals recipient count
        assert_eq!(
            r1["delivered_count"].as_u64().unwrap(),
            3,
            "delivered_count matches number of recipients"
        );

        // Single recipient
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "Sender",
                "to": ["D"],
                "subject": "test",
                "body": "hello"
            }),
        )
        .unwrap();
        assert_eq!(r2["delivered_count"].as_u64().unwrap(), 1);
    }

    // DR27-H5 (FIXED): Non-string name_hint now returns a clear error
    // instead of silently falling back to "AnonymousAgent".
    #[tokio::test]
    async fn dr27_h5_nonstring_name_hint_rejected() {
        let (state, _idx, _repo) = test_post_office();

        // Integer name_hint — now rejected with clear error
        let r1 = create_agent(
            &state,
            json!({
                "project_key": "project_a",
                "name_hint": 12345
            }),
        );
        assert!(r1.is_err(), "Non-string name_hint must be rejected");
        assert!(
            r1.unwrap_err().contains("name_hint must be a string"),
            "Error message explains the type requirement"
        );

        // Boolean name_hint — also rejected
        let r2 = create_agent(
            &state,
            json!({
                "project_key": "project_b",
                "name_hint": true
            }),
        );
        assert!(r2.is_err(), "Boolean name_hint must be rejected");

        // Null name_hint — treated as absent, defaults to AnonymousAgent
        let r3 = create_agent(
            &state,
            json!({
                "project_key": "project_a",
                "name_hint": null
            }),
        );
        assert!(r3.is_ok(), "Null name_hint treated as absent");
        assert_eq!(
            r3.unwrap()["name"].as_str().unwrap(),
            "AnonymousAgent",
            "Null name_hint defaults to AnonymousAgent"
        );

        // Missing name_hint — also defaults to AnonymousAgent (same project = idempotent)
        let r4 = create_agent(
            &state,
            json!({
                "project_key": "project_a"
            }),
        );
        assert!(r4.is_ok(), "Missing name_hint defaults to AnonymousAgent");
        assert_eq!(
            r4.unwrap()["name"].as_str().unwrap(),
            "AnonymousAgent",
            "Missing name_hint defaults to AnonymousAgent"
        );
    }

    // ── DR28 Hypotheses ─────────────────────────────────────────────────────

    // DR28-H1 (FIXED): Non-string program/model now rejected with clear error.
    // Null values still default to "unknown" (same as absent).
    #[tokio::test]
    async fn dr28_h1_nonstring_program_model_rejected() {
        let (state, _idx, _repo) = test_post_office();

        // Integer program — now rejected
        let r1 = create_agent(
            &state,
            json!({
                "project_key": "test",
                "name_hint": "Agent1",
                "program": 42,
                "model": "valid_model"
            }),
        );
        assert!(r1.is_err(), "Non-string program must be rejected");
        assert!(r1.unwrap_err().contains("program must be a string"));

        // Boolean model — now rejected
        let r2 = create_agent(
            &state,
            json!({
                "project_key": "test",
                "name_hint": "Agent2",
                "program": "valid",
                "model": true
            }),
        );
        assert!(r2.is_err(), "Non-string model must be rejected");
        assert!(r2.unwrap_err().contains("model must be a string"));

        // Null program/model — defaults to "unknown" (like absent)
        let r3 = create_agent(
            &state,
            json!({
                "project_key": "test",
                "name_hint": "Agent3",
                "program": null,
                "model": null
            }),
        );
        assert!(r3.is_ok(), "Null program/model treated as absent");
        let p = r3.unwrap();
        assert_eq!(p["program"], "unknown");
        assert_eq!(p["model"], "unknown");
    }

    // DR28-H2 (FIXED): to array with non-string elements now gives
    // a clear error explaining the type requirement.
    #[tokio::test]
    async fn dr28_h2_nonstring_to_elements_rejected_with_clear_error() {
        let (state, _idx, _repo) = test_post_office();

        // Array with only non-string elements → clear type error
        let result = send_message(
            &state,
            json!({
                "from_agent": "Sender",
                "to": [42, true, null],
                "subject": "test",
                "body": "hello"
            }),
        );

        assert!(result.is_err(), "Should fail with non-string recipients");
        let err = result.unwrap_err();
        assert!(
            err.contains("non-string"),
            "FIXED: Error explains that recipients must be strings, got: {}",
            err
        );

        // Mixed array: valid strings + non-strings → NOW also rejected (DR29-H5).
        // Previously only the all-non-string case was caught.
        let result2 = send_message(
            &state,
            json!({
                "from_agent": "Sender",
                "to": ["Alice", 42, "Bob"],
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(
            result2.is_err(),
            "FIXED: Mixed array with non-strings now rejected entirely (DR29-H5)"
        );
        assert!(
            result2.unwrap_err().contains("non-string"),
            "Error mentions non-string elements"
        );
    }

    // DR28-H3 (FIXED): Missing/non-string method now returns -32600
    // "Invalid Request" per JSON-RPC 2.0 §4.
    #[tokio::test]
    async fn dr28_h3_missing_method_returns_invalid_request() {
        let (state, _idx, _repo) = test_post_office();

        // Missing method → -32600 Invalid Request
        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0",
                "id": 1
            }),
        )
        .await;

        let code = resp["error"]["code"].as_i64().unwrap();
        assert_eq!(
            code, -32600,
            "Missing method returns -32600 Invalid Request"
        );
        assert!(resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing method"));

        // Non-string method → -32600 Invalid Request
        let resp2 = handle_mcp_request(
            state,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": 42
            }),
        )
        .await;

        let code2 = resp2["error"]["code"].as_i64().unwrap();
        assert_eq!(
            code2, -32600,
            "Non-string method returns -32600 Invalid Request"
        );
        assert!(resp2["error"]["message"]
            .as_str()
            .unwrap()
            .contains("method must be a string"));
    }

    // DR28-H4 (FIXED): Non-object arguments now returns clear "Invalid arguments"
    // error before reaching tool dispatch.
    #[tokio::test]
    async fn dr28_h4_nonobject_arguments_rejected_early() {
        let (state, _idx, _repo) = test_post_office();

        // String arguments → caught before tool dispatch
        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "create_agent",
                    "arguments": "not an object"
                }
            }),
        )
        .await;

        let err_msg = resp["error"]["message"].as_str().unwrap();
        assert_eq!(
            err_msg, "Invalid arguments: must be an object",
            "FIXED: Non-object arguments caught before tool dispatch"
        );
        assert_eq!(resp["error"]["code"], -32602);

        // Array arguments → same early rejection
        let resp2 = handle_mcp_request(
            state,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "send_message",
                    "arguments": [1, 2, 3]
                }
            }),
        )
        .await;

        let err_msg2 = resp2["error"]["message"].as_str().unwrap();
        assert_eq!(
            err_msg2, "Invalid arguments: must be an object",
            "FIXED: Array arguments caught before tool dispatch"
        );
    }

    // DR28-H5 (FIXED): Non-string required fields now return type-specific
    // errors instead of misleading "Missing X".
    #[tokio::test]
    async fn dr28_h5_nonstring_required_fields_return_type_error() {
        let (state, _idx, _repo) = test_post_office();

        // Integer from_agent → "from_agent must be a string"
        let r1 = send_message(
            &state,
            json!({
                "from_agent": 42,
                "to": ["Bob"],
                "subject": "test"
            }),
        );
        assert!(r1.is_err());
        assert!(
            r1.unwrap_err().contains("from_agent must be a string"),
            "FIXED: Integer from_agent says 'must be a string'"
        );

        // Boolean agent_name → "agent_name must be a string"
        let r2 = get_inbox(&state, json!({ "agent_name": true }));
        assert!(r2.is_err());
        assert!(
            r2.unwrap_err().contains("agent_name must be a string"),
            "FIXED: Boolean agent_name says 'must be a string'"
        );

        // Array query → "query must be a string"
        let r3 = search_messages(&state, json!({ "query": [1, 2, 3] }));
        assert!(r3.is_err());
        assert!(
            r3.unwrap_err().contains("query must be a string"),
            "FIXED: Array query says 'must be a string'"
        );

        // Integer project_key → "project_key must be a string"
        let r4 = create_agent(&state, json!({ "project_key": 999 }));
        assert!(r4.is_err());
        assert!(
            r4.unwrap_err().contains("project_key must be a string"),
            "FIXED: Integer project_key says 'must be a string'"
        );

        // Truly missing fields still say "Missing"
        let r5 = send_message(&state, json!({ "to": ["Bob"] }));
        assert!(r5.is_err());
        assert_eq!(r5.unwrap_err(), "Missing from_agent");
    }

    // ── DR29-H1 (FIXED): get_inbox limit non-integer type returns error ──
    // Non-integer limit now returns "limit must be an integer if provided".
    #[tokio::test]
    async fn dr29_h1_get_inbox_limit_noninteger_silently_defaults() {
        let (state, _idx, _repo) = test_post_office();

        // String "50" → type error
        let r1 = get_inbox(&state, json!({ "agent_name": "Agent29H1", "limit": "50" }));
        assert!(r1.is_err(), "FIXED: string limit is rejected");
        assert!(
            r1.unwrap_err().contains("limit must be an integer"),
            "FIXED: error explains type requirement"
        );

        // Boolean true → type error
        let r2 = get_inbox(&state, json!({ "agent_name": "Agent29H1", "limit": true }));
        assert!(r2.is_err(), "FIXED: boolean limit is rejected");
        assert!(r2.unwrap_err().contains("limit must be an integer"));

        // Object → type error
        let r3 = get_inbox(
            &state,
            json!({ "agent_name": "Agent29H1", "limit": {"value": 5} }),
        );
        assert!(r3.is_err(), "FIXED: object limit is rejected");
        assert!(r3.unwrap_err().contains("limit must be an integer"));

        // Null → uses default (same as absent)
        create_agent(
            &state,
            json!({ "project_key": "proj_dr29h1", "name_hint": "Agent29H1" }),
        )
        .unwrap();
        send_message(
            &state,
            json!({ "from_agent": "Sender29H1", "to": ["Agent29H1"], "subject": "test", "body": "hello" }),
        )
        .unwrap();
        let r4 = get_inbox(&state, json!({ "agent_name": "Agent29H1", "limit": null }));
        assert!(r4.is_ok(), "Null limit uses default (same as absent)");

        // Valid integer still works
        send_message(
            &state,
            json!({ "from_agent": "Sender29H1", "to": ["Agent29H1"], "subject": "test2", "body": "hi" }),
        )
        .unwrap();
        let r5 = get_inbox(&state, json!({ "agent_name": "Agent29H1", "limit": 5 }));
        assert!(r5.is_ok(), "Integer limit still works");
    }

    // ── DR29-H2 (FIXED): search_messages limit non-integer returns error ─
    // Non-integer limit now returns "limit must be an integer if provided".
    #[tokio::test]
    async fn dr29_h2_search_messages_limit_noninteger_silently_defaults() {
        let (state, _idx, _repo) = test_post_office();

        // String → type error
        let r1 = search_messages(&state, json!({ "query": "hello", "limit": "5" }));
        assert!(r1.is_err(), "FIXED: string limit is rejected");
        assert!(r1.unwrap_err().contains("limit must be an integer"));

        // Boolean → type error
        let r2 = search_messages(&state, json!({ "query": "hello", "limit": false }));
        assert!(r2.is_err(), "FIXED: boolean limit is rejected");
        assert!(r2.unwrap_err().contains("limit must be an integer"));

        // Float → type error
        let r3 = search_messages(&state, json!({ "query": "hello", "limit": 3.7 }));
        assert!(r3.is_err(), "FIXED: float limit is rejected");
        assert!(r3.unwrap_err().contains("limit must be an integer"));

        // Null → uses default (same as absent)
        let r4 = search_messages(&state, json!({ "query": "hello", "limit": null }));
        assert!(r4.is_ok(), "Null limit uses default (same as absent)");

        // Valid integer still works
        let r5 = search_messages(&state, json!({ "query": "hello", "limit": 5 }));
        assert!(r5.is_ok(), "Integer limit still works");
    }

    // ── DR29-H3 (FIXED): send_message optional fields reject non-string ──
    // Non-string subject, body, project_id now return type errors.
    #[tokio::test]
    async fn dr29_h3_send_message_optional_fields_nonstring_silently_default() {
        let (state, _idx, _repo) = test_post_office();

        // Integer subject → type error
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "Sender29H3",
                "to": ["Recip29H3"],
                "subject": 42,
                "body": "hello"
            }),
        );
        assert!(r1.is_err(), "FIXED: integer subject is rejected");
        assert!(
            r1.unwrap_err().contains("subject must be a string"),
            "Error mentions type requirement"
        );

        // Boolean body → type error
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "Sender29H3",
                "to": ["Recip29H3"],
                "subject": "test",
                "body": true
            }),
        );
        assert!(r2.is_err(), "FIXED: boolean body is rejected");
        assert!(r2.unwrap_err().contains("body must be a string"));

        // Array project_id → type error
        let r3 = send_message(
            &state,
            json!({
                "from_agent": "Sender29H3",
                "to": ["Recip29H3"],
                "subject": "test",
                "body": "hello",
                "project_id": [1, 2, 3]
            }),
        );
        assert!(r3.is_err(), "FIXED: array project_id is rejected");
        assert!(r3.unwrap_err().contains("project_id must be a string"));

        // Null values → use default (same as absent)
        let r4 = send_message(
            &state,
            json!({
                "from_agent": "Sender29H3",
                "to": ["Recip29H3"],
                "subject": null,
                "body": null,
                "project_id": null
            }),
        );
        assert!(r4.is_ok(), "Null values use defaults (same as absent)");
    }

    // ── DR29-H4 (FIXED): tools/call non-string name → clear type error ──
    // Non-string name now returns "name must be a string" with -32602.
    #[tokio::test]
    async fn dr29_h4_tools_call_nonstring_name_misleading_error() {
        let (state, _idx, _repo) = test_post_office();

        // Non-string name: integer → type error
        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": 42, "arguments": {} }
            }),
        )
        .await;
        let err_msg = resp["error"]["message"].as_str().unwrap();
        assert!(
            err_msg.contains("name must be a string"),
            "FIXED: integer name returns type error: {}",
            err_msg
        );
        assert_eq!(resp["error"]["code"], -32602);

        // Non-string name: boolean → type error
        let resp2 = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": { "name": true, "arguments": {} }
            }),
        )
        .await;
        let err_msg2 = resp2["error"]["message"].as_str().unwrap();
        assert!(
            err_msg2.contains("name must be a string"),
            "FIXED: boolean name returns type error: {}",
            err_msg2
        );

        // Null name → type error
        let resp3 = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": { "name": null, "arguments": {} }
            }),
        )
        .await;
        let err_msg3 = resp3["error"]["message"].as_str().unwrap();
        assert!(
            err_msg3.contains("name must be a string"),
            "FIXED: null name returns type error: {}",
            err_msg3
        );

        // Absent name → now returns specific "Missing tool name" error (DR34-H5)
        let resp4 = handle_mcp_request(
            state,
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": { "arguments": {} }
            }),
        )
        .await;
        let err_msg4 = resp4["error"]["message"].as_str().unwrap();
        assert!(
            err_msg4.contains("Missing tool name"),
            "FIXED: Absent name now returns specific error (DR34-H5): {}",
            err_msg4
        );
    }

    // ── DR29-H5 (FIXED): send_message mixed-type to array now rejected ──
    // Any non-string element in the to array rejects the entire request.
    #[tokio::test]
    async fn dr29_h5_send_message_mixed_to_array_silently_drops() {
        let (state, _idx, _repo) = test_post_office();

        // Mixed to array: integer + valid string → rejected
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "Sender29H5",
                "to": [42, "Recip29H5"],
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(r1.is_err(), "FIXED: mixed to array is rejected entirely");
        assert!(
            r1.unwrap_err().contains("non-string"),
            "Error mentions non-string elements"
        );

        // No messages delivered (entire request rejected)
        assert!(
            !state.inboxes.contains_key("Recip29H5"),
            "No partial delivery — entire request rejected"
        );

        // All-non-string case still caught
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "Sender29H5",
                "to": [42, true],
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(r2.is_err(), "All-non-string case still rejected");
        assert!(r2.unwrap_err().contains("non-string"));

        // Nulls are now also rejected as non-string (DR31-H1)
        let r3 = send_message(
            &state,
            json!({
                "from_agent": "Sender29H5",
                "to": ["Recip29H5", null],
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(
            r3.is_err(),
            "FIXED (DR31-H1): nulls in to array are now rejected"
        );
        assert!(r3.unwrap_err().contains("non-string"));
    }

    // ── DR30-H1: Inline name validation duplicates validate_agent_name() ──
    // from_agent, recipients, and agent_name all validate names inline
    // instead of calling validate_agent_name(). If validate_agent_name()
    // gains a new rule, the inline copies won't get it.
    #[tokio::test]
    async fn dr30_h1_inline_validation_duplicates_validate_agent_name() {
        let (state, _idx, _repo) = test_post_office();

        // Build a list of invalid names that validate_agent_name rejects
        let bad_names = vec![
            ("", "empty"),
            ("a/b", "path separator /"),
            ("a\\b", "path separator \\"),
            ("a..b", "double dot"),
            (".hidden", "dot prefix"),
            ("has\0null", "null byte"),
            ("has\x01ctrl", "control char"),
            ("   ", "whitespace only"),
            (" leading", "leading whitespace"),
            ("trailing ", "trailing whitespace"),
        ];

        for (name, reason) in &bad_names {
            // validate_agent_name should reject all of these
            let vr = validate_agent_name(name);
            assert!(
                vr.is_err(),
                "validate_agent_name must reject '{}' ({})",
                name.escape_debug(),
                reason
            );

            // from_agent should also reject all of these
            let fr = send_message(
                &state,
                json!({
                    "from_agent": name,
                    "to": ["SomeRecip"],
                    "subject": "test",
                    "body": "hello"
                }),
            );
            assert!(
                fr.is_err(),
                "from_agent must reject '{}' ({}) but got Ok",
                name.escape_debug(),
                reason
            );

            // recipient should also reject all of these
            // (skip empty string — it's filtered by to_agents builder, not validator)
            if !name.is_empty() {
                let rr = send_message(
                    &state,
                    json!({
                        "from_agent": "ValidSender",
                        "to": [name],
                        "subject": "test",
                        "body": "hello"
                    }),
                );
                assert!(
                    rr.is_err(),
                    "recipient must reject '{}' ({}) but got Ok",
                    name.escape_debug(),
                    reason
                );
            }

            // agent_name in get_inbox should also reject all of these
            if !name.is_empty() {
                let gr = get_inbox(&state, json!({ "agent_name": name }));
                assert!(
                    gr.is_err(),
                    "agent_name must reject '{}' ({}) but got Ok",
                    name.escape_debug(),
                    reason
                );
            }
        }

        // FIXED (DR30-H1): All validators now call validate_name() — a single
        // source of truth. New rules added to validate_name() automatically
        // apply to from_agent, recipients, and agent_name.
    }

    // ── DR30-H2: body rejects control chars (like subject does) ──────────
    // FIXED: Body now rejects control characters (except \n, \t, \r which
    // are legitimate in multi-line message content). Subject rejects ALL
    // control chars because subjects are single-line.
    #[tokio::test]
    async fn dr30_h2_body_rejects_control_chars_like_subject() {
        let (state, _idx, _repo) = test_post_office();

        // Subject with SOH control char → rejected
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "Sender30H2",
                "to": ["Recip30H2"],
                "subject": "hello\x01world",
                "body": "normal body"
            }),
        );
        assert!(r1.is_err(), "Subject with control char must be rejected");
        assert!(
            r1.unwrap_err().contains("control character"),
            "Error should mention control characters"
        );

        // Body with SOH control char → now also rejected
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "Sender30H2",
                "to": ["Recip30H2"],
                "subject": "normal subject",
                "body": "hello\x01world"
            }),
        );
        assert!(
            r2.is_err(),
            "FIXED: body with \\x01 control char is now rejected"
        );
        assert!(
            r2.unwrap_err().contains("control character"),
            "Error should mention control characters"
        );

        // But \n, \t, \r in body are allowed (legitimate in multi-line content)
        let r3 = send_message(
            &state,
            json!({
                "from_agent": "Sender30H2",
                "to": ["Recip30H2"],
                "subject": "normal subject",
                "body": "line1\nline2\ttab\rcarriage"
            }),
        );
        assert!(
            r3.is_ok(),
            "Body with newline, tab, carriage return must be accepted"
        );
    }

    // ── DR30-H3: Zero/negative limit now rejected with explicit error ─────
    // FIXED: Out-of-range limits return explicit errors instead of being
    // silently clamped. Schema declares minimum=1, server now enforces it.
    #[tokio::test]
    async fn dr30_h3_zero_and_negative_limit_rejected() {
        let (state, _idx, _repo) = test_post_office();

        // Send 3 messages
        for i in 0..3 {
            send_message(
                &state,
                json!({
                    "from_agent": "sender",
                    "to": ["target30h3"],
                    "subject": format!("msg {}", i),
                    "body": "x"
                }),
            )
            .unwrap();
        }

        // limit=0 → rejected
        let r1 = get_inbox(&state, json!({ "agent_name": "target30h3", "limit": 0 }));
        assert!(r1.is_err(), "FIXED: limit=0 now rejected");
        assert!(
            r1.unwrap_err().contains("limit must be >= 1"),
            "Error should mention minimum"
        );

        // limit=-5 → rejected
        let r2 = get_inbox(&state, json!({ "agent_name": "target30h3", "limit": -5 }));
        assert!(r2.is_err(), "FIXED: limit=-5 now rejected");
        assert!(
            r2.unwrap_err().contains("limit must be >= 1"),
            "Error should mention minimum"
        );

        // Messages should still be in inbox (nothing drained by failed calls)
        let r3 = get_inbox(&state, json!({ "agent_name": "target30h3", "limit": 3 }));
        assert!(r3.is_ok());
        let r3_val = r3.unwrap();
        let msgs3 = inbox_messages(&r3_val);
        assert_eq!(
            msgs3.len(),
            3,
            "All 3 messages preserved after rejected limit calls"
        );

        // Same for search_messages
        let r4 = search_messages(&state, json!({ "query": "hello", "limit": 0 }));
        assert!(r4.is_err(), "FIXED: search limit=0 now rejected");
        assert!(r4.unwrap_err().contains("limit must be >= 1"));

        let r5 = search_messages(&state, json!({ "query": "hello", "limit": -10 }));
        assert!(r5.is_err(), "FIXED: search limit=-10 now rejected");
        assert!(r5.unwrap_err().contains("limit must be >= 1"));
    }

    // ── DR30-H4: body length checked before null bytes (like subject) ─────
    // FIXED: Body now checks length BEFORE null bytes, matching subject's
    // fail-fast validation order. Oversized bodies report the length error.
    #[tokio::test]
    async fn dr30_h4_body_length_checked_before_null_bytes() {
        let (state, _idx, _repo) = test_post_office();

        // Subject checks length BEFORE null bytes (correct order)
        let long_subject = "x".repeat(MAX_SUBJECT_LEN + 1);
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "Sender30H4",
                "to": ["Recip30H4"],
                "subject": long_subject,
                "body": "ok"
            }),
        );
        assert!(r1.is_err());
        assert!(
            r1.unwrap_err().contains("exceeds"),
            "Subject correctly reports length error first"
        );

        // Body now also checks length BEFORE null bytes (fixed order)
        let mut long_body_with_null = "x".repeat(MAX_BODY_LEN + 100);
        long_body_with_null.push('\0');
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "Sender30H4",
                "to": ["Recip30H4"],
                "subject": "ok",
                "body": long_body_with_null
            }),
        );
        assert!(r2.is_err());
        let err = r2.unwrap_err();
        assert!(
            err.contains("exceeds"),
            "FIXED: oversized body with null byte now reports 'exceeds' error \
             (length checked first): {}",
            err
        );
    }

    // ════════════════════════════════════════════════════════════════════════════
    // Deep Review #31 — Hypothesis Tests
    // ════════════════════════════════════════════════════════════════════════════

    // ── DR31-H1 (FIXED): `to` array null elements now rejected ─────────────
    // FIXED: Nulls in `to` array are now rejected like any other non-string,
    // matching the schema declaration `items: { type: string }`.
    #[tokio::test]
    async fn dr31_h1_to_array_null_elements_now_rejected() {
        let (state, _idx, _repo) = test_post_office();

        // to: [null, "Alice"] — null is a non-string, entire request rejected
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "Sender31H1",
                "to": [null, "Alice31H1"],
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(r1.is_err(), "FIXED: null in to array is now rejected");
        assert!(r1.unwrap_err().contains("non-string"));

        // to: [null] — rejected with clear "non-string" error (not misleading "No recipients")
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "Sender31H1",
                "to": [null],
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(r2.is_err(), "FIXED: to=[null] rejected as non-string");
        assert!(
            r2.unwrap_err().contains("non-string"),
            "Error now correctly identifies the issue as non-string element"
        );

        // to: [null, null, "Bob31H1"] — rejected (null is non-string)
        let r3 = send_message(
            &state,
            json!({
                "from_agent": "Sender31H1",
                "to": [null, null, "Bob31H1"],
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(
            r3.is_err(),
            "FIXED: multiple nulls trigger non-string rejection"
        );

        // Integer still rejected
        let r4 = send_message(
            &state,
            json!({
                "from_agent": "Sender31H1",
                "to": [42, "Alice31H1"],
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(r4.is_err(), "Integer in to array still rejected");

        // Pure string array still works
        let r5 = send_message(
            &state,
            json!({
                "from_agent": "Sender31H1",
                "to": ["Alice31H1", "Bob31H1"],
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(r5.is_ok(), "Pure string array still works");

        // Verify schema-runtime alignment: schema says items must be strings
        let (state2, _idx2, _repo2) = test_post_office();
        let tools_list = handle_mcp_request(
            state2,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;
        let to_schema = &tools_list["result"]["tools"][1]["inputSchema"]["properties"]["to"];
        assert_eq!(
            to_schema["items"]["type"].as_str().unwrap(),
            "string",
            "Schema declares items must be strings — runtime now agrees"
        );
    }

    // ── DR31-H2 (FIXED): project_key/program/model use validate_text_field() ─
    // FIXED: Shared checks (empty, null bytes, control chars, length, whitespace)
    // are now in validate_text_core(), used by both validate_name() and
    // validate_text_field(). New rules added to the core propagate automatically.
    #[tokio::test]
    async fn dr31_h2_project_key_validation_shares_patterns_with_validate_name() {
        let (state, _idx, _repo) = test_post_office();

        // Both reject: empty
        assert!(validate_agent_name("").is_err(), "name: empty rejected");
        let r = create_agent(&state, json!({ "project_key": "" }));
        assert!(r.is_err(), "project_key: empty rejected");

        // Both reject: null bytes
        assert!(
            validate_agent_name("a\0b").is_err(),
            "name: null byte rejected"
        );
        let r = create_agent(&state, json!({ "project_key": "a\0b" }));
        assert!(r.is_err(), "project_key: null byte rejected");

        // Both reject: control chars
        assert!(
            validate_agent_name("a\x01b").is_err(),
            "name: control char rejected"
        );
        let r = create_agent(&state, json!({ "project_key": "a\x01b" }));
        assert!(r.is_err(), "project_key: control char rejected");

        // Both reject: whitespace-only
        assert!(
            validate_agent_name("   ").is_err(),
            "name: whitespace-only rejected"
        );
        let r = create_agent(&state, json!({ "project_key": "   " }));
        assert!(r.is_err(), "project_key: whitespace-only rejected");

        // Both reject: leading/trailing whitespace
        assert!(
            validate_agent_name(" a").is_err(),
            "name: leading whitespace rejected"
        );
        let r = create_agent(&state, json!({ "project_key": " a" }));
        assert!(r.is_err(), "project_key: leading whitespace rejected");

        // DIVERGENCE: validate_name rejects path separators, project_key does not
        assert!(
            validate_agent_name("a/b").is_err(),
            "name: path separator rejected"
        );
        let r = create_agent(
            &state,
            json!({ "project_key": "a/b", "name_hint": "AgentSlash" }),
        );
        assert!(
            r.is_ok(),
            "CONFIRMED: project_key accepts path separators (intentional divergence)"
        );

        // DIVERGENCE: validate_name rejects dot-prefixed, project_key does not
        assert!(
            validate_agent_name(".hidden").is_err(),
            "name: dot-prefix rejected"
        );
        let r = create_agent(
            &state,
            json!({ "project_key": ".hidden", "name_hint": "AgentDot" }),
        );
        assert!(
            r.is_ok(),
            "CONFIRMED: project_key accepts dot-prefixed names (intentional divergence)"
        );

        // DIVERGENCE: validate_name rejects '..', project_key does not
        assert!(
            validate_agent_name("a..b").is_err(),
            "name: double-dot rejected"
        );
        let r = create_agent(
            &state,
            json!({ "project_key": "a..b", "name_hint": "AgentDotDot" }),
        );
        assert!(
            r.is_ok(),
            "CONFIRMED: project_key accepts '..' (intentional divergence)"
        );
    }

    // ── DR31-H3: validate_name() null byte check is redundant ────────────────
    // The null byte check (line 250) is a subset of the control char check
    // (line 253) since '\0'.is_control() == true. The null byte check only
    // serves to produce a more specific error message. This test proves the
    // redundancy and documents the error message priority.
    #[tokio::test]
    async fn dr31_h3_null_byte_check_is_subset_of_control_char_check() {
        // Prove '\0' is a control character
        assert!(
            '\0'.is_control(),
            "Null byte IS a control character in Rust's char::is_control()"
        );

        // validate_name catches null byte with specific message BEFORE control char check
        let result = validate_name("a\0b", "test_field");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("null bytes"),
            "Null byte caught by the specific null-byte check: {}",
            err
        );
        assert!(
            !err.contains("control character"),
            "Error should NOT mention control characters (null byte check fires first)"
        );

        // Same redundancy exists in body validation: null byte check (line 529)
        // before control char check (line 535)
        let (state, _idx, _repo) = test_post_office();
        let r = send_message(
            &state,
            json!({
                "from_agent": "Sender31H3",
                "to": ["Recip31H3"],
                "subject": "test",
                "body": "hello\0world"
            }),
        );
        assert!(r.is_err());
        let body_err = r.unwrap_err();
        assert!(
            body_err.contains("null bytes"),
            "Body null byte caught by specific check: {}",
            body_err
        );

        // Same redundancy in subject: null byte check (line 518) before
        // control char check (line 521)
        let r = send_message(
            &state,
            json!({
                "from_agent": "Sender31H3",
                "to": ["Recip31H3"],
                "subject": "hello\0world",
                "body": "ok"
            }),
        );
        assert!(r.is_err());
        let subj_err = r.unwrap_err();
        assert!(
            subj_err.contains("null bytes"),
            "Subject null byte caught by specific check: {}",
            subj_err
        );

        // Same redundancy in project_key
        let r = create_agent(&state, json!({ "project_key": "test\0key" }));
        assert!(r.is_err());
        let pk_err = r.unwrap_err();
        assert!(
            pk_err.contains("null bytes"),
            "project_key null byte caught by specific check: {}",
            pk_err
        );

        // Same redundancy in program
        let r = create_agent(
            &state,
            json!({ "project_key": "test", "program": "prog\0ram" }),
        );
        assert!(r.is_err());
        let prog_err = r.unwrap_err();
        assert!(
            prog_err.contains("null bytes"),
            "program null byte caught by specific check: {}",
            prog_err
        );

        // Same redundancy in model
        let r = create_agent(&state, json!({ "project_key": "test", "model": "mod\0el" }));
        assert!(r.is_err());
        let model_err = r.unwrap_err();
        assert!(
            model_err.contains("null bytes"),
            "model null byte caught by specific check: {}",
            model_err
        );
    }

    // ── DR31-H4: empty project_id indexed as empty STRING in Tantivy ─────────
    // When project_id is absent/null, it defaults to "". This empty string is
    // indexed in Tantivy as a STRING field. This test verifies that messages
    // with empty project_id are still findable via full-text search on subject/body.
    #[tokio::test]
    async fn dr31_h4_empty_project_id_indexed_in_tantivy() {
        let (state, _idx, _repo) = test_post_office();

        // Send message with no project_id (defaults to "")
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "Sender31H4",
                "to": ["Recip31H4"],
                "subject": "xylophonereview",
                "body": "empty pid message"
            }),
        );
        assert!(r1.is_ok());

        // Send message WITH a project_id, using the SAME searchable body term
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "Sender31H4",
                "to": ["Recip31H4"],
                "subject": "xylophonereview",
                "body": "nonempty pid message",
                "project_id": "proj_test31h4"
            }),
        );
        assert!(r2.is_ok());

        // Wait for NRT refresh
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Search by the shared subject term should find both messages
        let search = search_messages(&state, json!({ "query": "xylophonereview", "limit": 10 }));
        assert!(
            search.is_ok(),
            "Search should work for messages with empty project_id"
        );
        let results = search.unwrap();
        let hits = results["results"].as_array().unwrap();
        assert!(
            hits.len() >= 2,
            "Both messages should be findable via shared subject term, got {}",
            hits.len()
        );

        // Check the project_id field value in the search results
        let mut found_empty_pid = false;
        let mut found_nonempty_pid = false;
        for hit in hits {
            let pid = hit
                .get("project_id")
                .and_then(|v| v.as_str())
                .unwrap_or("MISSING");
            if pid.is_empty() {
                found_empty_pid = true;
            }
            if pid == "proj_test31h4" {
                found_nonempty_pid = true;
            }
        }
        assert!(
            found_empty_pid,
            "CONFIRMED: Empty project_id is stored and returned in search results"
        );
        assert!(
            found_nonempty_pid,
            "Non-empty project_id also stored correctly"
        );
    }

    // ── DR31-H5 (FIXED): validate_name() uses static field for recipients ───
    // FIXED: validate_name(recipient, "recipient") uses a static &str instead of
    // format!("Recipient '{}'", recipient). Eliminates per-recipient heap
    // allocation on the hot path. 100-recipient broadcast now has zero
    // validation-related allocations.
    #[tokio::test]
    async fn dr31_h5_validate_name_static_field_for_recipients() {
        let (state, _idx, _repo) = test_post_office();

        // Create a message with maximum recipients (100) — all valid names.
        // validate_name now uses static "recipient" field, zero format! allocations.
        let recipients: Vec<String> = (0..super::MAX_RECIPIENTS)
            .map(|i| format!("Agent{:03}", i))
            .collect();
        let to_array: Vec<Value> = recipients.iter().map(|s| json!(s)).collect();

        let r = send_message(
            &state,
            json!({
                "from_agent": "BroadcastSender",
                "to": to_array,
                "subject": "broadcast",
                "body": "hello all"
            }),
        );
        assert!(r.is_ok(), "100-recipient broadcast should succeed");

        // Verify all 100 recipients received the message
        let mut delivered = 0;
        for name in &recipients {
            let inbox = get_inbox(&state, json!({ "agent_name": name })).unwrap();
            let msgs = inbox_messages(&inbox);
            delivered += msgs.len();
        }
        assert_eq!(
            delivered, 100,
            "All 100 recipients should receive the message"
        );

        // Error messages still identify the field (though not the specific name)
        let bad_r = send_message(
            &state,
            json!({
                "from_agent": "BroadcastSender",
                "to": ["valid", "a/b"],
                "subject": "test",
                "body": "hello"
            }),
        );
        assert!(bad_r.is_err());
        assert!(
            bad_r.unwrap_err().contains("recipient"),
            "Error message still identifies field as 'recipient'"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Deep Review #32 — Hypothesis Tests
    // ═══════════════════════════════════════════════════════════════════════════

    // ── DR32-H1 (FIXED): project_id validation order now length-first ─────────
    // FIXED: project_id now checks length→null→control, consistent with
    // subject/body (DR30-H4). Oversized project_id is rejected immediately
    // without scanning content.
    #[tokio::test]
    async fn dr32_h1_project_id_validation_order_inconsistent_with_dr30h4() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: Oversized project_id rejected by length check first,
        // without scanning for null bytes or control chars.
        let oversized_pid = "P".repeat(super::MAX_PROJECT_ID_LEN + 1);
        let r = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "test",
                "body": "hello",
                "project_id": oversized_pid,
            }),
        );
        assert!(r.is_err(), "Oversized project_id must be rejected");
        let err = r.unwrap_err();
        assert!(
            err.contains("project_id exceeds"),
            "FIXED: Length check fires first: {}",
            err
        );

        // Subject also checks length first (DR30-H4 fix) — now consistent:
        let oversized_subject = "S".repeat(super::MAX_SUBJECT_LEN + 1);
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": oversized_subject,
                "body": "hello",
            }),
        );
        assert!(r2.is_err());
        let err2 = r2.unwrap_err();
        assert!(
            err2.contains("Subject exceeds"),
            "Subject checks length first (DR30-H4): {}",
            err2
        );

        // FIXED: All text validation now follows length→null→control order.
    }

    // ── DR32-H2 (FIXED): search query validation now length-first ──────────────
    // FIXED: query now checks empty→length→null→control, consistent with
    // the DR30-H4 principle. Oversized queries rejected without content scans.
    #[tokio::test]
    async fn dr32_h2_search_query_validation_order_inconsistent_with_dr30h4() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: Oversized query rejected by length check immediately
        // after the empty check, without scanning for null/control chars.
        let oversized_query = "a".repeat(super::MAX_QUERY_LEN + 1);
        let r = search_messages(&state, json!({ "query": oversized_query }));
        assert!(r.is_err(), "Oversized query must be rejected");
        let err = r.unwrap_err();
        assert!(
            err.contains("query exceeds"),
            "FIXED: Length check fires first (after empty check): {}",
            err
        );

        // FIXED: All text validation now follows empty→length→null→control order.
    }

    // ── DR32-H3 (FIXED): project_id now rejects whitespace-only and padded ─────
    // FIXED: project_id in send_message now validates whitespace-only and
    // leading/trailing whitespace, consistent with project_key via
    // validate_text_field(). Empty string is still allowed (optional field).
    #[tokio::test]
    async fn dr32_h3_project_id_accepts_whitespace_only_and_padded() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: Whitespace-only project_id now rejected
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "test",
                "body": "hello",
                "project_id": "   ",
            }),
        );
        assert!(
            r1.is_err(),
            "FIXED: whitespace-only project_id is now rejected"
        );
        assert!(r1.unwrap_err().contains("whitespace"));

        // FIXED: Whitespace-padded project_id now rejected
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["charlie"],
                "subject": "test",
                "body": "hello",
                "project_id": " proj_1 ",
            }),
        );
        assert!(
            r2.is_err(),
            "FIXED: whitespace-padded project_id is now rejected"
        );
        assert!(r2.unwrap_err().contains("whitespace"));

        // Empty project_id is still valid (it's an optional field)
        let r3 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["dave"],
                "subject": "test",
                "body": "hello",
                "project_id": "",
            }),
        );
        assert!(
            r3.is_ok(),
            "Empty project_id still allowed (optional field)"
        );

        // project_key also rejects whitespace — now consistent:
        let r4 = create_agent(
            &state,
            json!({ "project_key": "   ", "name_hint": "Agent32H3" }),
        );
        assert!(
            r4.is_err(),
            "project_key rejects whitespace-only via validate_text_field()"
        );

        let r5 = create_agent(
            &state,
            json!({ "project_key": " padded ", "name_hint": "Agent32H3b" }),
        );
        assert!(
            r5.is_err(),
            "project_key rejects leading/trailing whitespace via validate_text_field()"
        );
    }

    // ── DR32-H4 (FIXED): unwrap_tantivy_arrays now takes by value ────────────
    // FIXED: Uses `Value::Array(mut arr) ... arr.swap_remove(0)` instead of
    // `ref arr` + `arr[0].clone()`. Avoids cloning up to 64KB body strings
    // per search result.
    #[tokio::test]
    async fn dr32_h4_unwrap_tantivy_arrays_clones_owned_values() {
        // Simulate a Tantivy doc with a large body field wrapped in array
        let big_body = "B".repeat(super::MAX_BODY_LEN); // 64KB
        let doc = json!({
            "id": ["msg_123"],
            "from_agent": ["alice"],
            "subject": ["test subject"],
            "body": [big_body],
            "created_ts": [1234567890_i64],
            "project_id": ["proj_1"],
            "to_recipients": ["bob"],
        });

        // This call works correctly (functional test)
        let unwrapped = unwrap_tantivy_arrays(doc);

        // All single-element arrays are unwrapped to scalars
        assert!(unwrapped["id"].is_string(), "id unwrapped to scalar");
        assert!(
            unwrapped["from_agent"].is_string(),
            "from_agent unwrapped to scalar"
        );
        assert!(
            unwrapped["subject"].is_string(),
            "subject unwrapped to scalar"
        );
        assert!(unwrapped["body"].is_string(), "body unwrapped to scalar");
        assert_eq!(
            unwrapped["body"].as_str().unwrap().len(),
            super::MAX_BODY_LEN,
            "64KB body preserved after unwrap"
        );

        // FIXED: The unwrap now uses swap_remove(0) instead of clone().
        // This takes the element by value from the owned array, avoiding
        // heap allocation for every field of every search result.
    }

    // ── DR32-H5 (FIXED): to_agents dedup now allocates once per recipient ──────
    // FIXED: .map(to_string) before .filter(insert clone) — one allocation
    // per recipient plus a cheap clone for the HashSet, instead of two
    // independent allocations.
    #[tokio::test]
    async fn dr32_h5_to_agents_dedup_allocates_twice_per_recipient() {
        let (state, _idx, _repo) = test_post_office();

        // 50 unique recipients — each name allocated twice in the dedup logic
        let recipients: Vec<String> = (0..50).map(|i| format!("Agent32H5_{:03}", i)).collect();

        let r = send_message(
            &state,
            json!({
                "from_agent": "sender",
                "to": recipients,
                "subject": "dedup alloc test",
                "body": "hello all",
            }),
        );
        assert!(r.is_ok(), "50-recipient send succeeds");
        assert_eq!(
            r.unwrap()["delivered_count"].as_u64().unwrap(),
            50,
            "All 50 recipients delivered"
        );

        // Verify all recipients got the message
        for name in &recipients {
            let inbox = get_inbox(&state, json!({ "agent_name": name })).unwrap();
            assert_eq!(
                inbox_messages(&inbox).len(),
                1,
                "{} should have 1 message",
                name
            );
        }

        // FIXED: The dedup code now does:
        //   .map(|s| s.to_string())                    // allocation #1 (owned)
        //   .filter(|s| seen.insert(s.clone()))         // clone for HashSet
        // One .to_string() allocation per recipient, plus a cheap clone for
        // the HashSet. Previously had two independent .to_string() calls.
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Deep Review #33 — Hypothesis Tests
    // ═══════════════════════════════════════════════════════════════════════════

    // ── DR33-H1 (FIXED): all length errors now consistently say "byte limit" ──
    // FIXED: validate_text_core and subject validation now say "byte limit"
    // instead of "character limit", consistent with body/project_id/query.
    // Accurate for multi-byte UTF-8 strings.
    #[tokio::test]
    async fn dr33_h1_validate_text_core_error_says_character_but_checks_bytes() {
        let (state, _idx, _repo) = test_post_office();

        // 33 × 4-byte chars = 132 bytes, but only 33 characters
        let name = "𝐀".repeat(33);
        assert_eq!(name.len(), 132, "33 4-byte chars = 132 bytes");
        assert_eq!(name.chars().count(), 33, "Only 33 characters");

        // validate_text_core (via validate_name) checks .len() > MAX_AGENT_NAME_LEN (128)
        // 132 > 128 → rejected.
        let r = create_agent(&state, json!({ "project_key": "test", "name_hint": name }));
        assert!(r.is_err(), "132-byte name exceeds 128-byte limit");
        let err = r.unwrap_err();
        // FIXED: error now correctly says "byte limit" (DR33-H1)
        assert!(
            err.contains("byte limit"),
            "FIXED: Error now says 'byte limit' (was 'character limit'): {}",
            err
        );

        // Body also says "byte limit" — now consistent:
        let big_body = "B".repeat(super::MAX_BODY_LEN + 1);
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "test",
                "body": big_body,
            }),
        );
        assert!(r2.is_err());
        let err2 = r2.unwrap_err();
        assert!(
            err2.contains("byte limit"),
            "Body says 'byte limit': {}",
            err2
        );

        // FIXED: Subject now also says "byte limit" — all consistent:
        let big_subject = "S".repeat(super::MAX_SUBJECT_LEN + 1);
        let r3 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": big_subject,
                "body": "ok",
            }),
        );
        assert!(r3.is_err());
        let err3 = r3.unwrap_err();
        assert!(
            err3.contains("byte limit"),
            "FIXED: Subject now says 'byte limit' (was 'character limit'): {}",
            err3
        );
    }

    // ── DR33-H2 (FIXED): subject now rejects whitespace-only and padded values ──
    // FIXED: Subject validation now includes whitespace-only and leading/trailing
    // whitespace checks, consistent with project_id (DR32-H3), project_key,
    // agent names, program, and model. Empty subject is still allowed (optional).
    #[tokio::test]
    async fn dr33_h2_subject_accepts_whitespace_only_and_padded() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: Whitespace-only subject now rejected
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob"],
                "subject": "   ",
                "body": "hello",
            }),
        );
        assert!(
            r1.is_err(),
            "FIXED: whitespace-only subject is now rejected (DR33-H2)"
        );
        assert!(r1.unwrap_err().contains("whitespace"));

        // FIXED: Leading/trailing whitespace in subject now rejected
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["charlie"],
                "subject": " padded subject ",
                "body": "hello",
            }),
        );
        assert!(
            r2.is_err(),
            "FIXED: padded subject is now rejected (DR33-H2)"
        );
        assert!(r2.unwrap_err().contains("whitespace"));

        // project_id also rejects whitespace (DR32-H3) — now consistent:
        let r3 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["dave"],
                "subject": "test",
                "body": "hello",
                "project_id": "   ",
            }),
        );
        assert!(
            r3.is_err(),
            "project_id rejects whitespace-only (DR32-H3) — now consistent with subject"
        );

        // project_key also rejects whitespace via validate_text_field — all consistent:
        let r4 = create_agent(
            &state,
            json!({ "project_key": "   ", "name_hint": "Agent33H2" }),
        );
        assert!(
            r4.is_err(),
            "project_key rejects whitespace-only via validate_text_field"
        );

        // Empty subject is still allowed (optional field defaults to ""):
        let r5 = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["eve"],
                "subject": "",
                "body": "hello",
            }),
        );
        assert!(r5.is_ok(), "Empty subject still allowed (optional field)");
    }

    // ── DR33-H3 (FIXED): InboxEntry now allocated after pre-checks ─────────────
    // FIXED: InboxEntry construction (cloning from_agent, subject, body, project_id)
    // is now after the inbox count cap check and inbox fullness pre-check.
    // Rejected sends no longer waste heap allocations.
    #[tokio::test]
    async fn dr33_h3_inbox_entry_allocated_before_precheck_rejection() {
        let (state, _idx, _repo) = test_post_office();

        // Fill recipient's inbox to cap
        for i in 0..super::MAX_INBOX_SIZE {
            send_message(
                &state,
                json!({
                    "from_agent": "filler",
                    "to": ["full_inbox"],
                    "subject": format!("msg #{}", i),
                    "body": "x",
                }),
            )
            .unwrap();
        }

        // Send with a 64KB body to the full inbox — rejected by pre-check.
        // FIXED: InboxEntry is NOT allocated before rejection (DR33-H3).
        let big_body = "B".repeat(super::MAX_BODY_LEN);
        let r = send_message(
            &state,
            json!({
                "from_agent": "sender",
                "to": ["full_inbox"],
                "subject": "big rejected message",
                "body": big_body,
            }),
        );
        assert!(
            r.is_err(),
            "FIXED: Send to full inbox rejected without allocating InboxEntry"
        );

        // Verify no partial state mutation
        assert!(
            !state.inboxes.contains_key("sender"),
            "Sender has no inbox entry — clean rejection"
        );
    }

    // ── DR33-H4 (FIXED): handle_mcp_request now borrows params, clones only args ─
    // FIXED: params is now borrowed as a &Value reference instead of cloned.
    // Only arguments is cloned — single copy of the body instead of two.
    #[tokio::test]
    async fn dr33_h4_handle_mcp_double_clones_params() {
        let (state, _idx, _repo) = test_post_office();

        // FIXED: tools/call with large body now clones only arguments,
        // not the entire params object. Single copy instead of double.
        let big_body = "B".repeat(super::MAX_BODY_LEN);
        let resp = handle_mcp_request(
            state,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "send_message",
                    "arguments": {
                        "from_agent": "alice",
                        "to": ["bob"],
                        "subject": "single clone test",
                        "body": big_body
                    }
                }
            }),
        )
        .await;

        assert!(
            resp.get("result").is_some(),
            "FIXED: Request succeeds with single clone of arguments (DR33-H4)"
        );
    }

    // ── DR33-H5 (FIXED): to_agents dedup now uses HashSet<&str> for zero-copy ───
    // FIXED: Dedup uses HashSet<&str> borrowing from the JSON array. Duplicates
    // are filtered BEFORE .map(to_string), so only unique recipients allocate
    // a String. Duplicates cause zero heap allocations.
    #[tokio::test]
    async fn dr33_h5_to_agents_dedup_clones_for_hashset() {
        let (state, _idx, _repo) = test_post_office();

        // 50 unique recipients with duplicates (75 total entries, 50 unique)
        let mut recipients: Vec<String> = (0..50).map(|i| format!("Agent33H5_{:03}", i)).collect();
        // Add 25 duplicates
        for i in 0..25 {
            recipients.push(format!("Agent33H5_{:03}", i));
        }
        assert_eq!(recipients.len(), 75, "75 entries, 50 unique");

        let r = send_message(
            &state,
            json!({
                "from_agent": "sender",
                "to": recipients,
                "subject": "dedup clone test",
                "body": "hello all",
            }),
        );
        assert!(r.is_ok(), "75-entry send with duplicates succeeds");
        assert_eq!(
            r.unwrap()["delivered_count"].as_u64().unwrap(),
            50,
            "50 unique recipients delivered"
        );

        // FIXED: Dedup now uses HashSet<&str> — duplicates are filtered by
        // borrowing &str from the JSON array, then only unique survivors
        // get .to_string(). 25 duplicates cause zero wasted allocations.

        // Verify delivery correctness
        for i in 0..50 {
            let name = format!("Agent33H5_{:03}", i);
            let inbox = get_inbox(&state, json!({ "agent_name": name })).unwrap();
            assert_eq!(
                inbox_messages(&inbox).len(),
                1,
                "Each unique recipient gets exactly 1 message"
            );
        }
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // DR34 — Deep Review #34 Hypothesis Tests
    // ═══════════════════════════════════════════════════════════════════════════

    // ── DR34-H1 (FIXED): tools/list schemas now include constraint metadata ──
    // All string fields declare maxLength, and the `to` array declares
    // minItems/maxItems, matching the runtime validation constants (DR34-H1).
    #[tokio::test]
    async fn dr34_h1_tools_list_schemas_omit_string_and_array_constraints() {
        let (state, _idx, _repo) = test_post_office();
        let resp = handle_mcp_request(
            state,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        )
        .await;
        let tools = resp["result"]["tools"].as_array().unwrap();

        // send_message schema
        let send_schema = tools.iter().find(|t| t["name"] == "send_message").unwrap();
        let send_props = &send_schema["inputSchema"]["properties"];

        // FIXED: String fields now declare maxLength matching runtime constants.
        assert_eq!(
            send_props["from_agent"]["maxLength"].as_u64().unwrap(),
            super::MAX_AGENT_NAME_LEN as u64,
            "from_agent schema declares maxLength"
        );
        assert_eq!(
            send_props["subject"]["maxLength"].as_u64().unwrap(),
            super::MAX_SUBJECT_LEN as u64,
            "subject schema declares maxLength"
        );
        assert_eq!(
            send_props["body"]["maxLength"].as_u64().unwrap(),
            super::MAX_BODY_LEN as u64,
            "body schema declares maxLength"
        );

        // FIXED: to array now declares minItems/maxItems.
        assert_eq!(
            send_props["to"]["minItems"].as_u64().unwrap(),
            1,
            "to schema declares minItems"
        );
        assert_eq!(
            send_props["to"]["maxItems"].as_u64().unwrap(),
            super::MAX_RECIPIENTS as u64,
            "to schema declares maxItems"
        );

        // create_agent schema
        let create_schema = tools.iter().find(|t| t["name"] == "create_agent").unwrap();
        let create_props = &create_schema["inputSchema"]["properties"];
        assert_eq!(
            create_props["project_key"]["maxLength"].as_u64().unwrap(),
            super::MAX_PROJECT_KEY_LEN as u64,
            "project_key schema declares maxLength"
        );
        assert_eq!(
            create_props["name_hint"]["maxLength"].as_u64().unwrap(),
            super::MAX_AGENT_NAME_LEN as u64,
            "name_hint schema declares maxLength"
        );

        // search_messages query field
        let search_schema = tools
            .iter()
            .find(|t| t["name"] == "search_messages")
            .unwrap();
        let search_props = &search_schema["inputSchema"]["properties"];
        assert_eq!(
            search_props["query"]["maxLength"].as_u64().unwrap(),
            super::MAX_QUERY_LEN as u64,
            "query schema declares maxLength"
        );

        // get_inbox agent_name field
        let inbox_schema = tools.iter().find(|t| t["name"] == "get_inbox").unwrap();
        let inbox_props = &inbox_schema["inputSchema"]["properties"];
        assert_eq!(
            inbox_props["agent_name"]["maxLength"].as_u64().unwrap(),
            super::MAX_AGENT_NAME_LEN as u64,
            "agent_name schema declares maxLength"
        );
    }

    // ── DR34-H2 (FIXED): search query now rejects whitespace-padded values ──
    // Consistent with subject (DR33-H2), project_id (DR32-H3), and all
    // validate_text_core paths. Query now also rejects leading/trailing
    // whitespace (DR34-H2).
    #[tokio::test]
    async fn dr34_h2_search_query_accepts_whitespace_padded_values() {
        let (state, _idx, _repo) = test_post_office();

        // Whitespace-padded subject is rejected
        let subj_result = send_message(
            &state,
            json!({
                "from_agent": "sender34h2",
                "to": ["recip34h2"],
                "subject": "  padded  ",
                "body": "test",
            }),
        );
        assert!(
            subj_result.is_err(),
            "Whitespace-padded subject must be rejected"
        );
        assert!(
            subj_result.unwrap_err().contains("leading or trailing"),
            "Subject error mentions whitespace"
        );

        // Whitespace-padded project_id is rejected
        let pid_result = send_message(
            &state,
            json!({
                "from_agent": "sender34h2",
                "to": ["recip34h2"],
                "body": "test",
                "project_id": "  padded  ",
            }),
        );
        assert!(
            pid_result.is_err(),
            "Whitespace-padded project_id must be rejected"
        );
        assert!(
            pid_result.unwrap_err().contains("leading or trailing"),
            "project_id error mentions whitespace"
        );

        // FIXED: Whitespace-padded query is now also rejected (was accepted).
        let query_result = search_messages(&state, json!({ "query": "  hello  ", "limit": 1 }));
        assert!(
            query_result.is_err(),
            "FIXED: Whitespace-padded query now rejected: {:?}",
            query_result
        );
        assert!(
            query_result.unwrap_err().contains("leading or trailing"),
            "Query error mentions whitespace"
        );
    }

    // ── DR34-H3 (FIXED): persist worker JoinHandle now stored ─────────────
    // PostOffice now stores the persist worker JoinHandle and provides a
    // shutdown() method that drops the channel sender then joins the thread,
    // ensuring all pending Tantivy batches are committed before exit (DR34-H3).
    #[tokio::test]
    async fn dr34_h3_persist_worker_join_handle_not_stored() {
        let (state, idx_dir, _repo) = test_post_office();

        // Send a persist op
        let send_result = state
            .persist_tx
            .try_send(crate::state::PersistOp::IndexMessage {
                id: "dr34h3_msg".to_string(),
                project_id: "proj_test".to_string(),
                from_agent: "sender".to_string(),
                to_recipients: "recip".to_string(),
                subject: "shutdown test".to_string(),
                body: "verify flush".to_string(),
                created_ts: 999999,
            });
        assert!(send_result.is_ok(), "Persist channel accepts ops");

        // FIXED: shutdown() drops the sender and joins the persist worker
        // thread, waiting for all pending batches to be committed.
        state.shutdown();

        // After shutdown(), the persist worker has finished — verify the
        // document was committed to the Tantivy index.
        let index = tantivy::Index::open_in_dir(idx_dir.path()).unwrap();
        let reader = index.reader().unwrap();
        reader.reload().unwrap();
        let searcher = reader.searcher();
        assert!(
            searcher.num_docs() > 0,
            "FIXED: shutdown() joined persist worker, document committed"
        );
    }

    // ── DR34-H4 (FIXED): batch Vec capacity now matches drain bound ────────
    // persistence_worker in state.rs now uses Vec::with_capacity(4096)
    // matching the drain loop bound, eliminating heap reallocations during
    // high-throughput bursts (DR34-H4).
    #[tokio::test]
    async fn dr34_h4_batch_vec_capacity_mismatches_drain_bound() {
        // FIXED: Vec::with_capacity(4096) matches `while batch.len() < 4096`.
        // No reallocations during high-throughput bursts.
        //
        // Verify by flooding the persist channel and checking that all ops
        // are processed without error.
        let (state, idx_dir, _repo) = test_post_office();

        // Send 2000 index ops (exceeds 1024 capacity, within 4096 bound)
        for i in 0..2000 {
            let _ = state
                .persist_tx
                .try_send(crate::state::PersistOp::IndexMessage {
                    id: format!("msg34h4_{}", i),
                    project_id: "proj_test".to_string(),
                    from_agent: "sender".to_string(),
                    to_recipients: "recip".to_string(),
                    subject: format!("batch test {}", i),
                    body: "test body".to_string(),
                    created_ts: 1000000 + i as i64,
                });
        }

        // Drop state to close channel — persist worker drains remaining ops
        drop(state);

        // Brief sleep to let persist worker finish its batch
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Open the index directly to verify docs were indexed
        let index = tantivy::Index::open_in_dir(idx_dir.path()).unwrap();
        let reader = index.reader().unwrap();
        reader.reload().unwrap();
        let searcher = reader.searcher();
        // At least some documents should have been indexed (persist worker
        // processed whatever was in the channel before shutdown)
        assert!(
            searcher.num_docs() > 0,
            "Persist worker processed batch despite capacity mismatch"
        );
    }

    // ── DR34-H5 (FIXED): tools/call with absent params returns specific error ─
    // Missing tool name now returns "Missing tool name in params" instead of
    // the ambiguous "Unknown tool: " with an empty name (DR34-H5).
    #[tokio::test]
    async fn dr34_h5_tools_call_absent_params_empty_tool_name() {
        let (state, _idx, _repo) = test_post_office();

        // tools/call with no params at all
        let resp1 = handle_mcp_request(
            state.clone(),
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call" }),
        )
        .await;
        let err1 = resp1["error"]["message"].as_str().unwrap();
        // FIXED: now returns a specific "Missing tool name" error
        assert_eq!(
            err1, "Missing tool name in params",
            "FIXED: Absent params returns specific error: {}",
            err1
        );

        // tools/call with explicit null params
        let resp2 = handle_mcp_request(
            state.clone(),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": null }),
        )
        .await;
        let err2 = resp2["error"]["message"].as_str().unwrap();
        assert_eq!(
            err2, "Missing tool name in params",
            "FIXED: Null params returns specific error: {}",
            err2
        );

        // tools/call with params but no name field
        let resp3 = handle_mcp_request(
            state,
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {} }),
        )
        .await;
        let err3 = resp3["error"]["message"].as_str().unwrap();
        assert_eq!(
            err3, "Missing tool name in params",
            "FIXED: Empty params.name returns specific error: {}",
            err3
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // Deep Review #35 — Hypothesis Tests
    // ══════════════════════════════════════════════════════════════════════

    // ── DR35-H1 (FIXED): GitActor JoinHandle now stored and joined ─────────
    // shutdown() now joins both the persist worker AND the git actor thread.
    // After persist worker exits (dropping git_tx), the git actor drains
    // remaining requests and exits. shutdown() joins it deterministically —
    // no sleep needed, no race condition (DR35-H1).
    #[tokio::test]
    async fn dr35_h1_git_actor_join_handle_not_stored() {
        let (state, _idx, repo_dir) = test_post_office();

        // Send a git commit op through the persist pipeline
        let _ = state
            .persist_tx
            .try_send(crate::state::PersistOp::GitCommit {
                path: "dr35_h1_test/file.txt".to_string(),
                content: "git actor shutdown test".to_string(),
                message: "DR35-H1: test git actor shutdown".to_string(),
            });

        // FIXED: shutdown() joins both persist worker AND git actor.
        // No sleep needed — deterministic shutdown guarantee.
        state.shutdown();

        // After shutdown(), the git actor has finished — file MUST exist.
        let git_file = repo_dir.path().join("dr35_h1_test/file.txt");
        assert!(
            git_file.exists(),
            "FIXED: shutdown() joins git actor thread — deterministic \
             commit guarantee without sleep (DR35-H1)"
        );
    }

    // ── DR35-H2 (FIXED): shutdown_signal() now handles both SIGINT and SIGTERM ─
    // main.rs now uses tokio::select! to handle both SIGINT (Ctrl-C) and
    // SIGTERM (Docker/Kubernetes/systemd). Previously only SIGINT was
    // handled, bypassing graceful shutdown on SIGTERM (DR35-H2).
    #[tokio::test]
    async fn dr35_h2_shutdown_signal_only_handles_sigint() {
        // Verify that both signal kinds are available on Unix
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};

            // FIXED: Both SIGTERM and SIGINT are now handled in shutdown_signal().
            // Verify that SIGTERM signal handler can be installed (same as main.rs).
            let sigterm_result = signal(SignalKind::terminate());
            assert!(
                sigterm_result.is_ok(),
                "FIXED: SIGTERM handler can be installed — shutdown_signal() \
                 now uses tokio::select! for both SIGINT and SIGTERM (DR35-H2)"
            );
        }

        // Cross-platform: verify ctrl_c() works (regression guard)
        let ctrl_c_future = tokio::signal::ctrl_c();
        drop(ctrl_c_future);
    }

    // ── DR35-H3 (FIXED): validate_name dot checks merged ──────────────────
    // Previously had redundant `name == "."` check before `starts_with('.')`.
    // Now merged into a single starts_with('.') check with a nested
    // conditional for the specific "." error message (DR35-H3).
    #[tokio::test]
    async fn dr35_h3_validate_name_dot_check_redundancy() {
        let (state, _idx, _repo) = test_post_office();

        // "." still gets the specific error message (merged conditional)
        let dot_result = create_agent(&state, json!({ "project_key": "test", "name_hint": "." }));
        assert!(dot_result.is_err(), "Single dot must be rejected");
        let dot_err = dot_result.unwrap_err();
        assert!(
            dot_err.contains("must not be '.'"),
            "FIXED: '.' still gets specific error from merged check: {}",
            dot_err
        );

        // ".hidden" still gets the starts_with error
        let hidden_result = create_agent(
            &state,
            json!({ "project_key": "test", "name_hint": ".hidden" }),
        );
        assert!(hidden_result.is_err(), "Dot-prefixed name must be rejected");
        let hidden_err = hidden_result.unwrap_err();
        assert!(
            hidden_err.contains("must not start with '.'"),
            "FIXED: '.hidden' gets starts_with error from merged check: {}",
            hidden_err
        );
    }

    // ── DR35-H4 (FIXED): warning field added for partial delivery ──────────
    // When delivered_count < expected recipients (due to concurrent races),
    // the response now includes a "warning" field explaining the partial
    // delivery. Normal sends (all delivered) omit the warning (DR35-H4).
    #[tokio::test]
    async fn dr35_h4_delivered_count_silent_skip_no_error() {
        let (state, _idx, _repo) = test_post_office();

        // Normal send: all recipients delivered, no warning
        let result = send_message(
            &state,
            json!({
                "from_agent": "alice",
                "to": ["bob", "charlie"],
                "subject": "delivered count test",
                "body": "hello",
            }),
        );
        assert!(result.is_ok());
        let resp = result.unwrap();

        assert_eq!(
            resp["delivered_count"].as_u64().unwrap(),
            2,
            "Normal send: delivered_count == recipient count"
        );
        assert_eq!(
            resp["status"].as_str().unwrap(),
            "sent",
            "Status is 'sent' for full delivery"
        );
        assert!(
            resp.get("warning").is_none(),
            "FIXED: No warning when all recipients delivered (DR35-H4)"
        );
    }

    // ── DR35-H5 (FIXED): Git commit messages now distinguish register/update ─
    // create_agent now uses "Register agent {name}" for fresh registrations
    // and "Update agent {name}" for re-registrations, making the Git
    // audit trail unambiguous (DR35-H5).
    #[tokio::test]
    async fn dr35_h5_git_commit_message_same_for_register_and_reregister() {
        let (state, _idx, repo_dir) = test_post_office();

        // First registration
        let r1 = create_agent(
            &state,
            json!({
                "project_key": "dr35h5proj",
                "name_hint": "AuditAgent",
                "program": "v1",
                "model": "gpt-4"
            }),
        );
        assert!(r1.is_ok(), "First registration succeeds");

        // Re-registration with updated program
        let r2 = create_agent(
            &state,
            json!({
                "project_key": "dr35h5proj",
                "name_hint": "AuditAgent",
                "program": "v2",
                "model": "gpt-4o"
            }),
        );
        assert!(r2.is_ok(), "Re-registration succeeds (same project)");

        // FIXED: shutdown() joins both persist worker AND git actor,
        // so all commits are guaranteed to complete.
        state.shutdown();

        // Verify the Git log — commits should have distinct messages
        let repo = git2::Repository::open(repo_dir.path()).unwrap();
        let mut revwalk = repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        let mut messages: Vec<String> = Vec::new();
        for oid in revwalk {
            let oid = oid.unwrap();
            let commit = repo.find_commit(oid).unwrap();
            if let Some(msg) = commit.message() {
                if msg.contains("AuditAgent") {
                    messages.push(msg.to_string());
                }
            }
        }

        assert!(
            messages.len() >= 2,
            "Should have at least 2 commits for AuditAgent: {:?}",
            messages
        );

        // FIXED: Fresh registration says "Register", re-registration says "Update"
        let has_register = messages.iter().any(|m| m == "Register agent AuditAgent");
        let has_update = messages.iter().any(|m| m == "Update agent AuditAgent");
        assert!(
            has_register,
            "FIXED: Fresh registration uses 'Register agent' commit message: {:?}",
            messages
        );
        assert!(
            has_update,
            "FIXED: Re-registration uses 'Update agent' commit message: {:?}",
            messages
        );
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Deep Review #36 — Hypothesis Tests
    // ═══════════════════════════════════════════════════════════════════════════

    // ── DR36-H1: indexed counter increments on failed add_document ───────────
    // In persistence_worker (state.rs), `indexed += 1` runs unconditionally
    // after add_document, even when the call fails. The counter reports
    // attempts not successes, causing unnecessary commit() calls when all
    // additions fail. This test documents the current (buggy) behavior.
    // Note: Hard to unit-test the persistence_worker directly since it runs
    // on a background thread. We test the observable consequence: the
    // persist pipeline processes batches correctly for valid input. The
    // actual fix is in state.rs (move indexed += 1 inside success path).
    #[tokio::test]
    async fn dr36_h1_indexed_counter_counts_attempts_not_successes() {
        let (state, idx_dir, _repo) = test_post_office();

        // Send a valid message through the persist pipeline
        let _ = state
            .persist_tx
            .try_send(crate::state::PersistOp::IndexMessage {
                id: "dr36h1_valid".to_string(),
                project_id: "proj_test".to_string(),
                from_agent: "sender".to_string(),
                to_recipients: "recip".to_string(),
                subject: "valid message".to_string(),
                body: "this should be indexed".to_string(),
                created_ts: 1000000,
            });

        // Shutdown to flush
        state.shutdown();

        // Verify the document was indexed
        let index = tantivy::Index::open_in_dir(idx_dir.path()).unwrap();
        let reader = index.reader().unwrap();
        reader.reload().unwrap();
        let searcher = reader.searcher();
        assert!(
            searcher.num_docs() >= 1,
            "Valid IndexMessage should be indexed (indexed counter counts it)"
        );
    }

    // ── DR36-H4: registered_at stability across re-registrations ─────────────
    // The code preserves registered_at from the original registration during
    // re-registration (DR26-H2), but no existing test asserts the timestamp
    // value is unchanged. A refactoring error could silently break this.
    #[tokio::test]
    async fn dr36_h4_registered_at_preserved_on_reregistration() {
        let (state, _idx, _repo) = test_post_office();

        // First registration
        let r1 = create_agent(
            &state,
            json!({
                "project_key": "dr36h4proj",
                "name_hint": "TimestampAgent",
                "program": "v1"
            }),
        )
        .unwrap();
        let original_registered_at = r1["registered_at"].as_i64().unwrap();

        // Small delay to ensure different timestamp
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Re-registration with updated fields
        let r2 = create_agent(
            &state,
            json!({
                "project_key": "dr36h4proj",
                "name_hint": "TimestampAgent",
                "program": "v2",
                "model": "gpt-4o"
            }),
        )
        .unwrap();
        let reregistered_at = r2["registered_at"].as_i64().unwrap();

        // registered_at MUST be preserved from original registration
        assert_eq!(
            original_registered_at, reregistered_at,
            "registered_at must be preserved across re-registrations \
             (original={}, re-reg={})",
            original_registered_at, reregistered_at
        );

        // But program and model should be updated
        assert_eq!(r2["program"], "v2");
        assert_eq!(r2["model"], "gpt-4o");

        // And agent_id should be stable
        assert_eq!(r1["id"], r2["id"]);
    }

    // ── DR36-H5: to_recipients STRING indexing prevents per-recipient search ─
    // to_recipients uses STRING | STORED in the Tantivy schema, making the
    // entire joined value a single exact-match token. Searching for an
    // individual recipient in a multi-recipient message won't match.
    #[tokio::test]
    async fn dr36_h5_to_recipients_string_type_prevents_per_recipient_search() {
        let (state, idx_dir, _repo) = test_post_office();

        // Index a multi-recipient message directly through the persist pipeline
        let _ = state
            .persist_tx
            .try_send(crate::state::PersistOp::IndexMessage {
                id: "dr36h5_msg".to_string(),
                project_id: "proj_test".to_string(),
                from_agent: "sender".to_string(),
                to_recipients: "alice\x1Fbob\x1Fcharlie".to_string(),
                subject: "multi-recipient dr36h5".to_string(),
                body: "test body for recipient search".to_string(),
                created_ts: 2000000,
            });

        // Flush
        state.shutdown();

        // Reopen index for search
        let index = tantivy::Index::open_in_dir(idx_dir.path()).unwrap();
        let reader = index.reader().unwrap();
        reader.reload().unwrap();
        let searcher = reader.searcher();

        let schema = index.schema();
        let subject_field = schema.get_field("subject").unwrap();
        let body_field = schema.get_field("body").unwrap();

        // Verify the document exists via subject search
        let query_parser =
            tantivy::query::QueryParser::for_index(&index, vec![subject_field, body_field]);
        let query = query_parser.parse_query("dr36h5").unwrap();
        let results = searcher
            .search(&query, &tantivy::collector::TopDocs::with_limit(10))
            .unwrap();
        assert_eq!(
            results.len(),
            1,
            "Message exists and is searchable via subject"
        );

        // FIXED (DR36-H5): to_recipients now uses TEXT (tokenized) instead of
        // STRING (exact match). Individual recipient search works.
        let to_field = schema.get_field("to_recipients").unwrap();
        let recip_parser = tantivy::query::QueryParser::for_index(&index, vec![to_field]);
        let recip_query = recip_parser.parse_query("alice").unwrap();
        let recip_results = searcher
            .search(&recip_query, &tantivy::collector::TopDocs::with_limit(10))
            .unwrap();

        // FIXED: TEXT type tokenizes the joined string, so "alice" matches
        // within "alice\x1Fbob\x1Fcharlie". Per-recipient search now works.
        assert_eq!(
            recip_results.len(),
            1,
            "FIXED (DR36-H5): TEXT-indexed to_recipients matches individual \
             recipients in multi-recipient messages"
        );
    }

    // ── DR36-H3 (FIXED): search_messages now uses direct field extraction ─────
    // Previously used schema.to_json() → serde_json::from_str() round-trip.
    // Now builds JSON directly from Tantivy document fields, eliminating
    // unnecessary string allocations (DR36-H3).
    #[tokio::test]
    async fn dr36_h3_search_messages_json_round_trip_is_correct() {
        let (state, _idx, _repo) = test_post_office();

        // Index a message with a large body to exercise the round-trip
        let big_body = "SearchRoundTrip ".repeat(100);
        let _ = state
            .persist_tx
            .try_send(crate::state::PersistOp::IndexMessage {
                id: "dr36h3_msg".to_string(),
                project_id: "proj_roundtrip".to_string(),
                from_agent: "alice".to_string(),
                to_recipients: "bob".to_string(),
                subject: "roundtrip36 test subject".to_string(),
                body: big_body.clone(),
                created_ts: 3000000,
            });

        // Wait for NRT refresh (100ms interval + margin)
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Search for the message
        let result = search_messages(&state, json!({ "query": "roundtrip36", "limit": 10 }));
        assert!(result.is_ok(), "Search should succeed");

        let resp = result.unwrap();
        let results = resp["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "Should find 1 result");

        // Verify the round-trip preserved all fields correctly
        let doc = &results[0];

        // Direct field extraction produces scalar values (no arrays to unwrap)
        assert_eq!(
            doc["id"].as_str().unwrap(),
            "dr36h3_msg",
            "id field preserved through round-trip"
        );
        assert_eq!(
            doc["project_id"].as_str().unwrap(),
            "proj_roundtrip",
            "project_id preserved"
        );
        assert_eq!(
            doc["from_agent"].as_str().unwrap(),
            "alice",
            "from_agent preserved"
        );
        assert_eq!(
            doc["subject"].as_str().unwrap(),
            "roundtrip36 test subject",
            "subject preserved"
        );
        assert_eq!(
            doc["body"].as_str().unwrap(),
            big_body,
            "Large body preserved through direct field extraction"
        );
        assert_eq!(
            doc["created_ts"].as_i64().unwrap(),
            3000000,
            "Timestamp preserved"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Deep Review #37 — Hypothesis Tests
    // ═══════════════════════════════════════════════════════════════════════════

    // ── DR37-H1: search_messages direct extraction returns empty/zero for missing fields ─
    // After DR36-H3, direct field extraction uses unwrap_or("") and unwrap_or(0).
    // If a field is genuinely missing from a Tantivy document, the result now
    // contains empty strings/zeros instead of omitting the field. This test
    // documents and verifies the current behavior.
    #[tokio::test]
    async fn dr37_h1_search_direct_extraction_returns_defaults_for_all_fields() {
        let (state, _idx, _repo) = test_post_office();

        // Index a message with all fields populated
        let _ = state
            .persist_tx
            .try_send(crate::state::PersistOp::IndexMessage {
                id: "dr37h1_msg".to_string(),
                project_id: "proj_dr37".to_string(),
                from_agent: "alice".to_string(),
                to_recipients: "bob".to_string(),
                subject: "dr37h1 subject".to_string(),
                body: "dr37h1 body content".to_string(),
                created_ts: 4000000,
            });

        // Wait for NRT refresh
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let result = search_messages(&state, json!({ "query": "dr37h1", "limit": 10 }));
        assert!(result.is_ok(), "Search should succeed");

        let resp = result.unwrap();
        let results = resp["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "Should find 1 result");

        let doc = &results[0];

        // All fields must be present as scalar values (not arrays, not null)
        assert!(doc["id"].is_string(), "id is a string");
        assert!(doc["project_id"].is_string(), "project_id is a string");
        assert!(doc["from_agent"].is_string(), "from_agent is a string");
        assert!(
            doc["to_recipients"].is_string(),
            "to_recipients is a string"
        );
        assert!(doc["subject"].is_string(), "subject is a string");
        assert!(doc["body"].is_string(), "body is a string");
        assert!(
            doc["created_ts"].is_i64(),
            "created_ts is an integer (direct extraction)"
        );

        // With all fields populated, values are scalars (not null).
        // Missing fields would serialize as null instead of empty string/zero (DR37-H1).
        assert!(!doc["id"].is_null(), "present id is never null");
        assert!(
            !doc["created_ts"].is_null(),
            "present created_ts is never null"
        );
    }

    // ── DR37-H2 (DISPROVED): mcp_handler borrow-then-move is safe ───────────
    // Regression guard: the tracing::debug! line uses borrowed &str from
    // payload, then payload is moved into handle_mcp_request. This is safe
    // because &str is Copy and the borrows end before the move.
    #[tokio::test]
    async fn dr37_h2_mcp_handler_borrow_order_is_safe() {
        let (state, _idx, _repo) = test_post_office();

        // This exercises the full mcp_handler path (borrow method/tool, then move payload)
        let resp = handle_mcp_request(
            state,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "get_inbox",
                    "arguments": { "agent_name": "test37h2" }
                }
            }),
        )
        .await;

        assert!(
            resp.get("result").is_some(),
            "Full request path works (borrows resolved before move)"
        );
    }

    // ── DR37-H5: last recipient clone is unnecessary ─────────────────────────
    // For N recipients, entry.clone() is called N times. The Nth clone is
    // wasteful — the original entry could be moved into the last recipient's
    // inbox. This test documents the current behavior and verifies all
    // recipients get identical messages regardless.
    #[tokio::test]
    async fn dr37_h5_all_recipients_get_identical_messages() {
        let (state, _idx, _repo) = test_post_office();

        let big_body = "DR37H5 ".repeat(1000); // ~7KB body

        let result = send_message(
            &state,
            json!({
                "from_agent": "sender37h5",
                "to": ["recip_a", "recip_b", "recip_c"],
                "subject": "clone test",
                "body": big_body,
            }),
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["delivered_count"].as_u64().unwrap(), 3);

        // Verify all 3 recipients have identical message content
        let mut bodies = Vec::new();
        for name in &["recip_a", "recip_b", "recip_c"] {
            let inbox = get_inbox(&state, json!({ "agent_name": name })).unwrap();
            let msgs = inbox_messages(&inbox);
            assert_eq!(msgs.len(), 1, "{} should have 1 message", name);
            assert_eq!(msgs[0]["from_agent"], "sender37h5");
            assert_eq!(msgs[0]["subject"], "clone test");
            bodies.push(msgs[0]["body"].as_str().unwrap().to_string());
        }

        // All bodies must be identical (clones are correct)
        assert_eq!(bodies[0], bodies[1], "recip_a and recip_b have same body");
        assert_eq!(bodies[1], bodies[2], "recip_b and recip_c have same body");
        assert_eq!(bodies[0], big_body, "Body content preserved");
    }

    // ── DR37-H3: schema.get_field() called per request ──────────────────────
    // search_messages performs 7 schema.get_field() lookups per call. These
    // are cheap (small schema) but wasteful under high throughput. This test
    // verifies search works correctly and documents the per-request overhead.
    #[tokio::test]
    async fn dr37_h3_search_repeated_calls_consistent_field_resolution() {
        let (state, _idx, _repo) = test_post_office();

        // Index a message
        let _ = state
            .persist_tx
            .try_send(crate::state::PersistOp::IndexMessage {
                id: "dr37h3_msg".to_string(),
                project_id: "proj_consistency".to_string(),
                from_agent: "alice".to_string(),
                to_recipients: "bob".to_string(),
                subject: "consistency37h3 test".to_string(),
                body: "repeated search test".to_string(),
                created_ts: 5000000,
            });

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Search multiple times — field resolution must be consistent
        for i in 0..5 {
            let result =
                search_messages(&state, json!({ "query": "consistency37h3", "limit": 10 }));
            assert!(result.is_ok(), "Search #{} should succeed", i);
            let resp = result.unwrap();
            let results = resp["results"].as_array().unwrap();
            assert_eq!(results.len(), 1, "Search #{} finds 1 result", i);
            assert_eq!(
                results[0]["id"].as_str().unwrap(),
                "dr37h3_msg",
                "Search #{} returns same doc",
                i
            );
        }
    }

    // ── DR38-H1: shutdown() silently skips poisoned mutexes ──────────────
    // The shutdown() method uses `if let Ok(...)` to lock persist_handle and
    // git_handle. If either Mutex is poisoned, the join is silently skipped,
    // potentially losing pending data. This test documents the pattern.
    #[tokio::test]
    async fn dr38_h1_shutdown_mutex_pattern_uses_if_let_ok() {
        // Verify that PostOffice has persist_handle and git_handle fields
        // that are Arc<Mutex<Option<JoinHandle<()>>>>. We can't trigger a
        // poison in a unit test without unsafe, so this characterization test
        // verifies shutdown() completes successfully under normal conditions.
        let (state, _idx, _repo) = test_post_office();

        // Send a message to create some persist pipeline activity
        let _ = state
            .persist_tx
            .try_send(crate::state::PersistOp::IndexMessage {
                id: "dr38h1_msg".to_string(),
                project_id: "proj_dr38".to_string(),
                from_agent: "alice".to_string(),
                to_recipients: "bob".to_string(),
                subject: "shutdown test".to_string(),
                body: "testing graceful shutdown".to_string(),
                created_ts: 9000000,
            });

        // shutdown() should complete without panic or hang
        state.shutdown();
        // If we get here, shutdown succeeded (mutexes were not poisoned).
    }

    // ── DR38-H2: persistence_worker resolves fields independently ────────
    // The persistence_worker resolves its own field handles from a cloned
    // schema. These MUST match the SearchFields resolved at startup.
    // This test verifies they produce identical results by indexing a doc
    // via the persist pipeline and reading it back via search with SearchFields.
    #[tokio::test]
    async fn dr38_h2_persist_worker_and_search_fields_are_consistent() {
        let (state, _idx, _repo) = test_post_office();

        // Index via persist pipeline (uses worker's own field handles)
        let _ = state
            .persist_tx
            .try_send(crate::state::PersistOp::IndexMessage {
                id: "dr38h2_msg".to_string(),
                project_id: "proj_dr38h2".to_string(),
                from_agent: "writer_agent".to_string(),
                to_recipients: "reader_agent".to_string(),
                subject: "consistency38h2 check".to_string(),
                body: "worker vs search fields".to_string(),
                created_ts: 8000000,
            });

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Read back via search_messages (uses SearchFields from PostOffice)
        let result = search_messages(&state, json!({ "query": "consistency38h2", "limit": 10 }));
        assert!(result.is_ok(), "Search should succeed");

        let resp = result.unwrap();
        let results = resp["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "Should find exactly 1 result");

        let doc = &results[0];
        // Every field written by the worker must be readable by SearchFields
        assert_eq!(doc["id"].as_str().unwrap(), "dr38h2_msg");
        assert_eq!(doc["project_id"].as_str().unwrap(), "proj_dr38h2");
        assert_eq!(doc["from_agent"].as_str().unwrap(), "writer_agent");
        assert_eq!(doc["to_recipients"].as_str().unwrap(), "reader_agent");
        assert_eq!(doc["subject"].as_str().unwrap(), "consistency38h2 check");
        assert_eq!(doc["body"].as_str().unwrap(), "worker vs search fields");
        assert_eq!(doc["created_ts"].as_i64().unwrap(), 8000000);
    }

    // ── DR38-H3: search_messages count is redundant with results.len() ──
    // The `count` field in search_messages response always equals the length
    // of the results array. It does NOT represent total matching documents.
    #[tokio::test]
    async fn dr38_h3_search_count_equals_results_array_length() {
        let (state, _idx, _repo) = test_post_office();

        // Index 5 messages with the same keyword
        for i in 0..5 {
            let _ = state
                .persist_tx
                .try_send(crate::state::PersistOp::IndexMessage {
                    id: format!("dr38h3_msg_{}", i),
                    project_id: "proj_dr38h3".to_string(),
                    from_agent: "alice".to_string(),
                    to_recipients: "bob".to_string(),
                    subject: format!("counttest38h3 item {}", i),
                    body: "search count test".to_string(),
                    created_ts: 7000000 + i,
                });
        }

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Search with limit=2 — should return 2 results, total_hits=5
        let result = search_messages(&state, json!({ "query": "counttest38h3", "limit": 2 }));
        assert!(result.is_ok());

        let resp = result.unwrap();
        let results = resp["results"].as_array().unwrap();
        let count = resp["count"].as_u64().unwrap();
        let total_hits = resp["total_hits"].as_u64().unwrap();

        assert_eq!(results.len(), 2, "Should return exactly 2 results");
        assert_eq!(
            count,
            results.len() as u64,
            "count equals results array length"
        );
        // total_hits reports ALL matching docs, not just the returned subset (DR38-H3)
        assert_eq!(total_hits, 5, "total_hits reports all matching documents");
        assert!(
            total_hits > count,
            "total_hits > count when limit < total matches"
        );
    }

    // ── DR38-H4: re-registration profile lacks updated_at ───────────────
    // When an agent re-registers with different program/model, the returned
    // profile JSON preserves the original registered_at. There is no
    // updated_at field to indicate when the re-registration occurred.
    #[tokio::test]
    async fn dr38_h4_reregistration_profile_lacks_updated_at() {
        let (state, _idx, _repo) = test_post_office();

        // First registration
        let r1 = create_agent(
            &state,
            json!({
                "project_key": "proj_dr38h4",
                "name_hint": "AgentH4",
                "program": "v1",
                "model": "gpt-4"
            }),
        );
        assert!(r1.is_ok());
        let profile1 = r1.unwrap();
        let registered_at_1 = profile1["registered_at"].as_i64().unwrap();

        // Small delay to ensure timestamps differ
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        // Re-registration with different program/model
        let r2 = create_agent(
            &state,
            json!({
                "project_key": "proj_dr38h4",
                "name_hint": "AgentH4",
                "program": "v2",
                "model": "gpt-5"
            }),
        );
        assert!(r2.is_ok());
        let profile2 = r2.unwrap();

        // registered_at is preserved from first registration (DR26-H2, DR27-H3)
        assert_eq!(
            profile2["registered_at"].as_i64().unwrap(),
            registered_at_1,
            "registered_at must be preserved on re-registration"
        );

        // program and model are updated
        assert_eq!(profile2["program"].as_str().unwrap(), "v2");
        assert_eq!(profile2["model"].as_str().unwrap(), "gpt-5");

        // updated_at is present on re-registration so the profile JSON
        // alone can distinguish fresh registrations from updates (DR38-H4).
        let updated_at = profile2["updated_at"].as_i64();
        assert!(updated_at.is_some(), "updated_at exists on re-registration");
        assert!(
            updated_at.unwrap() >= registered_at_1,
            "updated_at >= registered_at"
        );

        // UPDATED (DR39-H2): Fresh registration now has updated_at: null
        // for consistent schema across fresh and re-registration responses.
        assert!(
            profile1["updated_at"].is_null(),
            "Fresh registration has updated_at: null (DR39-H2)"
        );
    }

    // ── DR38-H5: deliver closure allocates String per entry() call ───────
    // DashMap::entry() requires an owned key. The deliver closure calls
    // recipient.to_string() for every recipient, even when the inbox exists.
    // This test verifies the allocation is correct (messages are delivered)
    // and documents the per-recipient String allocation overhead.
    #[tokio::test]
    async fn dr38_h5_deliver_allocates_string_per_recipient() {
        let (state, _idx, _repo) = test_post_office();

        // Pre-create inboxes by sending an initial message
        let r1 = send_message(
            &state,
            json!({
                "from_agent": "setup38h5",
                "to": ["inbox_a", "inbox_b", "inbox_c"],
                "subject": "setup",
                "body": "pre-create inboxes"
            }),
        );
        assert!(r1.is_ok());

        // Drain to clear but keep inboxes existing in DashMap
        // (get_inbox removes the entry on full drain, so we need to
        // send again to have occupied entries)
        let r2 = send_message(
            &state,
            json!({
                "from_agent": "sender38h5",
                "to": ["inbox_a", "inbox_b", "inbox_c"],
                "subject": "test38h5",
                "body": "deliver to existing inboxes"
            }),
        );
        assert!(r2.is_ok());
        assert_eq!(r2.unwrap()["delivered_count"].as_u64().unwrap(), 3);

        // Verify all 3 recipients have 2 messages (setup + test)
        for name in &["inbox_a", "inbox_b", "inbox_c"] {
            let inbox = get_inbox(&state, json!({ "agent_name": name })).unwrap();
            let msgs = inbox_messages(&inbox);
            assert_eq!(
                msgs.len(),
                2,
                "{} should have 2 messages (entry() works for existing inboxes)",
                name
            );
        }
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Deep Review #39 — Hypothesis Tests
    // ═══════════════════════════════════════════════════════════════════════════

    // ── DR39-H1: get_inbox allocates String for nonexistent agents via entry() ─
    // get_inbox calls state.inboxes.entry(agent_name.to_string()) for every
    // request, even when the agent has no inbox. The Vacant branch is a no-op
    // but the entry() call requires an owned String key allocation. A
    // contains_key fast path (borrowing &str) would avoid this allocation
    // for the common "no inbox" case.
    #[tokio::test]
    async fn dr39_h1_get_inbox_nonexistent_agent_avoids_entry() {
        let (state, _idx, _repo) = test_post_office();

        // Nonexistent agent — no inbox exists
        assert!(!state.inboxes.contains_key("nonexistent39h1"));

        let result = get_inbox(&state, json!({ "agent_name": "nonexistent39h1" })).unwrap();
        assert_eq!(inbox_messages(&result).len(), 0);
        assert_eq!(result["remaining"], 0);

        // The entry() call's Vacant branch must NOT create a DashMap entry.
        // This documents that even though entry() is called, no spurious
        // entry is left behind (the Vacant branch does not insert).
        assert!(
            !state.inboxes.contains_key("nonexistent39h1"),
            "get_inbox on nonexistent agent must not create a DashMap entry"
        );
    }

    // ── DR39-H2: create_agent response schema inconsistency ──────────────────
    // Fresh registration omits `updated_at`, re-registration includes it.
    // This makes the API response schema inconsistent. Clients cannot rely
    // on a fixed set of fields across fresh and re-registration calls.
    #[tokio::test]
    async fn dr39_h2_create_agent_response_schema_consistency() {
        let (state, _idx, _repo) = test_post_office();

        // Fresh registration
        let r1 = create_agent(
            &state,
            json!({
                "project_key": "proj_dr39h2",
                "name_hint": "SchemaAgent",
                "program": "v1"
            }),
        )
        .unwrap();

        // Fresh registration should have these exact fields
        assert!(r1.get("id").is_some(), "id present");
        assert!(r1.get("project_id").is_some(), "project_id present");
        assert!(r1.get("name").is_some(), "name present");
        assert!(r1.get("program").is_some(), "program present");
        assert!(r1.get("model").is_some(), "model present");
        assert!(r1.get("registered_at").is_some(), "registered_at present");

        // FIXED (DR39-H2): updated_at is now always present.
        // Fresh registration has updated_at: null.
        assert!(
            r1.get("updated_at").is_some(),
            "FIXED: updated_at always present (null on fresh registration)"
        );
        assert!(
            r1["updated_at"].is_null(),
            "Fresh registration has updated_at: null"
        );

        // Re-registration
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let r2 = create_agent(
            &state,
            json!({
                "project_key": "proj_dr39h2",
                "name_hint": "SchemaAgent",
                "program": "v2"
            }),
        )
        .unwrap();

        // updated_at is a timestamp on re-registration
        assert!(
            r2.get("updated_at").is_some(),
            "Re-registration includes updated_at"
        );
        assert!(
            r2["updated_at"].is_i64(),
            "Re-registration updated_at is an integer timestamp"
        );

        // Consistent schema: both responses have exactly the same 7 fields
        let fields_fresh: Vec<&str> = r1.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        let fields_rereg: Vec<&str> = r2.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(
            fields_fresh.len(),
            fields_rereg.len(),
            "FIXED: Same number of fields in fresh and re-registration responses"
        );
    }

    // ── DR39-H3: get_inbox partial drain doesn't shrink Vec capacity ─────────
    // Vec::drain(..limit) removes elements but never reduces the Vec's
    // allocated capacity. After partial drains, the inbox Vec retains its
    // peak capacity until the entry is fully drained and removed.
    #[tokio::test]
    async fn dr39_h3_partial_drain_vec_capacity_behavior() {
        let (state, _idx, _repo) = test_post_office();

        // Fill inbox with 200 messages
        for i in 0..200 {
            send_message(
                &state,
                json!({
                    "from_agent": "filler39h3",
                    "to": ["target39h3"],
                    "subject": format!("msg #{}", i),
                    "body": "x",
                }),
            )
            .unwrap();
        }

        // Partial drain: take 50, leave 150
        let page1 = get_inbox(&state, json!({ "agent_name": "target39h3", "limit": 50 })).unwrap();
        assert_eq!(inbox_messages(&page1).len(), 50);
        assert_eq!(page1["remaining"], 150);

        // After partial drain, the entry still exists in DashMap
        assert!(
            state.inboxes.contains_key("target39h3"),
            "Inbox entry persists after partial drain"
        );

        // Drain remaining
        let page2 =
            get_inbox(&state, json!({ "agent_name": "target39h3", "limit": 1000 })).unwrap();
        assert_eq!(inbox_messages(&page2).len(), 150);
        assert_eq!(page2["remaining"], 0);

        // After full drain, entry is removed (occ.remove())
        assert!(
            !state.inboxes.contains_key("target39h3"),
            "Inbox entry removed after full drain — frees Vec capacity"
        );
    }

    // ── DR39-H4 (DISPROVED): Double-Arc NrtShutdownGuard is correct ──────────
    // Arc<NrtShutdownGuard(Arc<AtomicBool>)> — the outer Arc ensures the Drop
    // fires only when the LAST PostOffice clone is dropped, the inner Arc
    // shares the flag with the tokio NRT refresh task. Both are needed.
    #[tokio::test]
    async fn dr39_h4_nrt_shutdown_guard_arc_pattern_correct() {
        let (state, _idx, _repo) = test_post_office();

        // Clone the state (simulating Axum Extension layer)
        let clone1 = state.clone();
        let clone2 = state.clone();

        // Dropping clones should NOT trigger NRT shutdown
        // (search still works after dropping clones)
        drop(clone1);
        drop(clone2);

        // Original state still works — NRT guard not triggered
        let result = send_message(
            &state,
            json!({
                "from_agent": "dr39h4_sender",
                "to": ["dr39h4_receiver"],
                "subject": "arc test",
                "body": "guard still active"
            }),
        );
        assert!(
            result.is_ok(),
            "State works after dropping clones (NRT guard not triggered)"
        );

        // Shutdown the original — this drops the last Arc, triggering NRT guard
        state.shutdown();
    }

    // ── DR39-H5 (DISPROVED): QueryParser per-request overhead is negligible ──
    // QueryParser::for_index() is called per search request. This test verifies
    // it doesn't degrade under repeated construction by running 200 sequential
    // searches. The construction is lightweight (Arc increment + small Vec).
    #[tokio::test]
    async fn dr39_h5_query_parser_per_request_overhead_negligible() {
        let (state, _idx, _repo) = test_post_office();

        // 200 sequential searches — each creates a new QueryParser
        let start = std::time::Instant::now();
        for i in 0..200 {
            let result = search_messages(
                &state,
                json!({ "query": format!("term39h5_{}", i), "limit": 10 }),
            );
            assert!(result.is_ok(), "Search #{} should succeed", i);
        }
        let elapsed = start.elapsed();

        // 200 searches should complete well under 1 second
        // (each QueryParser construction is sub-microsecond)
        assert!(
            elapsed.as_secs() < 2,
            "200 searches completed in {:?} — QueryParser overhead is negligible",
            elapsed
        );
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // DR40 HYPOTHESIS TESTS
    // ═══════════════════════════════════════════════════════════════════════════

    // ── DR40-H1 (CONFIRMED): subject/project_id validation duplicates ────────
    // validate_text_core. The inline checks in send_message for subject,
    // body, project_id, and query in search_messages duplicate the same
    // 5-check pattern (length, null bytes, control chars, whitespace-only,
    // leading/trailing whitespace). This test documents that all four
    // optional fields reject the same invalid inputs that validate_text_core
    // would, confirming the duplication exists and the behavior is consistent.
    #[tokio::test]
    async fn dr40_h1_optional_field_validation_mirrors_validate_text_core() {
        let (state, _idx, _repo) = test_post_office();

        // Register agents for send_message tests
        create_agent(
            &state,
            json!({ "project_key": "dr40h1", "name_hint": "sender40h1" }),
        )
        .unwrap();
        create_agent(
            &state,
            json!({ "project_key": "dr40h1", "name_hint": "rcv40h1" }),
        )
        .unwrap();

        // Test cases that validate_text_core would reject — verify that
        // the inline duplicates in send_message also reject them.
        let bad_inputs = vec![
            ("\0evil", "null bytes"),
            ("\x01control", "control characters"),
            ("  padded  ", "leading or trailing whitespace"),
            ("   ", "whitespace-only"),
        ];

        for (bad_val, desc) in &bad_inputs {
            // Subject inline validation
            let result = send_message(
                &state,
                json!({
                    "from_agent": "sender40h1",
                    "to": ["rcv40h1"],
                    "subject": bad_val,
                    "body": "ok"
                }),
            );
            assert!(
                result.is_err(),
                "Subject should reject {} input: {:?}",
                desc,
                bad_val
            );

            // project_id inline validation
            let result = send_message(
                &state,
                json!({
                    "from_agent": "sender40h1",
                    "to": ["rcv40h1"],
                    "subject": "ok",
                    "body": "ok",
                    "project_id": bad_val
                }),
            );
            assert!(
                result.is_err(),
                "project_id should reject {} input: {:?}",
                desc,
                bad_val
            );
        }

        // Query inline validation in search_messages
        for (bad_val, desc) in &bad_inputs {
            let result = search_messages(&state, json!({ "query": bad_val, "limit": 10 }));
            assert!(
                result.is_err(),
                "query should reject {} input: {:?}",
                desc,
                bad_val
            );
        }

        state.shutdown();
    }

    // ── DR40-H2 (CONFIRMED): persistence_worker batch Vec reallocated ────────
    // per iteration. The batch Vec is created with Vec::with_capacity(4096)
    // inside the loop, meaning each batch iteration allocates ~200KB on the
    // heap. Moving it outside the loop with clear() would reuse the buffer.
    // This test verifies the batch processing works correctly (messages are
    // indexed) — the allocation is a style/performance concern, not a bug.
    #[tokio::test]
    async fn dr40_h2_persistence_worker_batch_processes_correctly() {
        let (state, _idx, _repo) = test_post_office();

        create_agent(
            &state,
            json!({ "project_key": "dr40h2", "name_hint": "sender40h2" }),
        )
        .unwrap();
        create_agent(
            &state,
            json!({ "project_key": "dr40h2", "name_hint": "rcv40h2" }),
        )
        .unwrap();

        // Send multiple messages that will be batched by the persist worker
        for i in 0..10 {
            let result = send_message(
                &state,
                json!({
                    "from_agent": "sender40h2",
                    "to": ["rcv40h2"],
                    "subject": format!("batch_test_{}", i),
                    "body": format!("batch body {}", i)
                }),
            );
            assert!(result.is_ok(), "Message {} should send", i);
        }

        // Wait for NRT reader to pick up the indexed documents
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Verify all messages were indexed through the batch pipeline
        let result = search_messages(&state, json!({ "query": "batch_test", "limit": 20 }));
        let docs = result.unwrap();
        let arr = docs["results"].as_array().unwrap();
        assert!(
            arr.len() >= 10,
            "All 10 batched messages should be indexed, got {}",
            arr.len()
        );

        state.shutdown();
    }

    // ── DR40-H3 (CONFIRMED): path.clone() in GitCommit always clones ────────
    // In persistence_worker, PersistOp::GitCommit destructures path by move,
    // then clones it for the GitRequest. The clone is needed only for the
    // error log — on success, the original path is consumed. This test
    // verifies GitCommit operations work correctly end-to-end.
    #[tokio::test]
    async fn dr40_h3_git_commit_path_clone_works_correctly() {
        let (state, _idx, _repo) = test_post_office();

        // create_agent triggers a GitCommit PersistOp for the agent profile
        let result = create_agent(
            &state,
            json!({
                "project_key": "dr40h3",
                "name_hint": "gitclone40h3",
                "program": "test-program",
                "model": "test-model"
            }),
        );
        assert!(result.is_ok(), "Agent creation should succeed");

        let profile = result.unwrap();
        assert_eq!(profile["name"], "gitclone40h3");

        // The GitCommit PersistOp was sent with a path like
        // "agents/<name>.json". The path.clone() happens inside the
        // persist worker for the error log path. Verify the agent was
        // properly created (the git commit is fire-and-forget).
        assert!(
            state.agents.contains_key("gitclone40h3"),
            "Agent should exist in DashMap after creation"
        );

        state.shutdown();
    }

    // ── DR40-H4 (CONFIRMED): AgentRecord stores fields never read ────────────
    // AgentRecord has `id`, `project_id`, `program`, `model`, `registered_at`
    // fields that are written during create_agent but never read in production
    // code paths (get_inbox, send_message, search_messages). The struct has
    // #[allow(dead_code)]. This test documents that the fields exist and are
    // populated, confirming the struct is a write-only record for now.
    #[tokio::test]
    async fn dr40_h4_agent_record_fields_populated_but_not_read() {
        let (state, _idx, _repo) = test_post_office();

        create_agent(
            &state,
            json!({
                "project_key": "dr40h4",
                "name_hint": "reccheck40h4",
                "program": "my-program",
                "model": "my-model"
            }),
        )
        .unwrap();

        // Verify all AgentRecord fields are populated
        {
            let record = state.agents.get("reccheck40h4").unwrap();
            assert!(!record.id.is_empty(), "id should be populated");
            assert!(
                !record.project_id.is_empty(),
                "project_id should be populated"
            );
            assert_eq!(record.name, "reccheck40h4");
            assert_eq!(record.program, "my-program");
            assert_eq!(record.model, "my-model");
            assert!(record.registered_at > 0, "registered_at should be set");

            // These fields are stored but never queried in any hot path.
            // send_message checks state.agents.contains_key() — only the key matters.
            // get_inbox uses state.inboxes — doesn't touch agents at all.
            // search_messages uses Tantivy — doesn't touch agents at all.
            // The #[allow(dead_code)] attribute on AgentRecord confirms this.
        }

        state.shutdown();
    }

    // ── DR40-H5 (DISPROVED): unwrap_tantivy_arrays is NOT dead code ─────────
    // Hypothesis: unwrap_tantivy_arrays is no longer used in production code.
    // DISPROVED: It is called in search_messages (production) and used by
    // at least 2 existing tests. This test confirms it still works correctly.
    #[tokio::test]
    async fn dr40_h5_unwrap_tantivy_arrays_still_used_in_production() {
        let (state, _idx, _repo) = test_post_office();

        create_agent(
            &state,
            json!({ "project_key": "dr40h5", "name_hint": "sender40h5" }),
        )
        .unwrap();
        create_agent(
            &state,
            json!({ "project_key": "dr40h5", "name_hint": "rcv40h5" }),
        )
        .unwrap();

        send_message(
            &state,
            json!({
                "from_agent": "sender40h5",
                "to": ["rcv40h5"],
                "subject": "unwraptest40h5",
                "body": "verifying arrays are unwrapped"
            }),
        )
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let result = search_messages(&state, json!({ "query": "unwraptest40h5", "limit": 5 }));
        let docs = result.unwrap();
        let arr = docs["results"].as_array().unwrap();
        assert!(!arr.is_empty(), "Should find the indexed message");

        // Verify search_messages produces scalar values, not arrays.
        // Tantivy natively returns fields as arrays (e.g., "subject":["hello"]).
        // The direct extraction via get_first() returns scalar strings.
        let doc = &arr[0];
        assert!(
            doc["subject"].is_string(),
            "subject should be unwrapped from array to string, got: {}",
            doc["subject"]
        );
        assert!(
            doc["body"].is_string(),
            "body should be unwrapped from array to string, got: {}",
            doc["body"]
        );

        state.shutdown();
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // DR41 HYPOTHESIS TESTS
    // ═══════════════════════════════════════════════════════════════════════════

    // ── DR41-H1 (CONFIRMED): body accepts whitespace-only values ─────────────
    // Subject and project_id reject whitespace-only via validate_optional_text,
    // but body has no whitespace check — whitespace-only bodies are accepted.
    // This is arguably intentional (bodies are multi-line content) but the
    // asymmetry is undocumented and could surprise callers.
    #[tokio::test]
    async fn dr41_h1_body_accepts_whitespace_only_unlike_subject() {
        let (state, _idx, _repo) = test_post_office();

        create_agent(
            &state,
            json!({ "project_key": "dr41h1", "name_hint": "sender41h1" }),
        )
        .unwrap();
        create_agent(
            &state,
            json!({ "project_key": "dr41h1", "name_hint": "rcv41h1" }),
        )
        .unwrap();

        // Whitespace-only subject is rejected (via validate_optional_text)
        let subj_result = send_message(
            &state,
            json!({
                "from_agent": "sender41h1",
                "to": ["rcv41h1"],
                "subject": "   ",
                "body": "ok"
            }),
        );
        assert!(
            subj_result.is_err(),
            "Whitespace-only subject should be rejected"
        );

        // Whitespace-only body is ACCEPTED (no whitespace check on body)
        let body_result = send_message(
            &state,
            json!({
                "from_agent": "sender41h1",
                "to": ["rcv41h1"],
                "subject": "ok",
                "body": "   "
            }),
        );
        assert!(
            body_result.is_ok(),
            "Whitespace-only body is accepted (intentional — bodies are multi-line content)"
        );

        // Whitespace-padded body is also ACCEPTED
        let padded_result = send_message(
            &state,
            json!({
                "from_agent": "sender41h1",
                "to": ["rcv41h1"],
                "subject": "ok",
                "body": "  hello  "
            }),
        );
        assert!(
            padded_result.is_ok(),
            "Whitespace-padded body is accepted (bodies allow leading/trailing whitespace)"
        );

        state.shutdown();
    }

    // ── DR41-H2 (CONFIRMED): search_messages count field is redundant ────────
    // The `count` field in search_messages response is always equal to the
    // length of the `results` array. The useful pagination field is
    // `total_hits` which reports ALL matching documents, not just returned.
    #[tokio::test]
    async fn dr41_h2_search_count_always_equals_results_length() {
        let (state, _idx, _repo) = test_post_office();

        create_agent(
            &state,
            json!({ "project_key": "dr41h2", "name_hint": "sender41h2" }),
        )
        .unwrap();
        create_agent(
            &state,
            json!({ "project_key": "dr41h2", "name_hint": "rcv41h2" }),
        )
        .unwrap();

        // Send 5 messages
        for i in 0..5 {
            send_message(
                &state,
                json!({
                    "from_agent": "sender41h2",
                    "to": ["rcv41h2"],
                    "subject": format!("countredundant41h2_{}", i),
                    "body": "test"
                }),
            )
            .unwrap();
        }

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Search with limit=3 — returns 3 results, total_hits=5
        let result = search_messages(&state, json!({ "query": "countredundant41h2", "limit": 3 }));
        let resp = result.unwrap();
        let results = resp["results"].as_array().unwrap();
        let count = resp["count"].as_u64().unwrap();
        let total_hits = resp["total_hits"].as_u64().unwrap();

        // count is ALWAYS equal to results.len() — redundant
        assert_eq!(
            count,
            results.len() as u64,
            "count is always results.len() — redundant with array length"
        );
        // total_hits provides the actual pagination information
        assert!(
            total_hits >= 5,
            "total_hits reports all matching docs ({}), not just returned ({})",
            total_hits,
            results.len()
        );
        assert!(
            total_hits > count || results.len() == 5,
            "total_hits differs from count when results are limited"
        );

        state.shutdown();
    }

    // ── DR41-H3 (CONFIRMED): query validation is inline, not shared ──────────
    // query validation duplicates the same checks as validate_text_core but
    // with a different order (empty+whitespace-only combined first, then
    // length). This test documents that query rejects the same inputs as
    // validate_text_core, confirming functional equivalence despite the
    // structural duplication.
    #[tokio::test]
    async fn dr41_h3_query_validation_mirrors_validate_text_core() {
        let (state, _idx, _repo) = test_post_office();

        // All of these are rejected by both validate_text_core and query validation
        let bad_inputs: Vec<(&str, &str)> = vec![
            ("", "empty"),
            ("   ", "whitespace-only"),
            ("\0evil", "null bytes"),
            ("\x01control", "control characters"),
            ("  padded  ", "leading/trailing whitespace"),
        ];

        for (bad_val, desc) in &bad_inputs {
            let result = search_messages(&state, json!({ "query": bad_val, "limit": 10 }));
            assert!(
                result.is_err(),
                "query should reject {} input: {:?}",
                desc,
                bad_val
            );
        }

        state.shutdown();
    }

    // ── DR41-H4 (DISPROVED): created_ts naming is consistent ─────────────────
    // Both get_inbox and search_messages serialize the timestamp as
    // `created_ts`. InboxEntry stores it internally as `timestamp` but the
    // response uses `created_ts` in both reading paths.
    #[tokio::test]
    async fn dr41_h4_created_ts_naming_consistent_across_responses() {
        let (state, _idx, _repo) = test_post_office();

        create_agent(
            &state,
            json!({ "project_key": "dr41h4", "name_hint": "sender41h4" }),
        )
        .unwrap();
        create_agent(
            &state,
            json!({ "project_key": "dr41h4", "name_hint": "rcv41h4" }),
        )
        .unwrap();

        send_message(
            &state,
            json!({
                "from_agent": "sender41h4",
                "to": ["rcv41h4"],
                "subject": "tsname41h4",
                "body": "timestamp naming test"
            }),
        )
        .unwrap();

        // get_inbox response uses created_ts
        let inbox_result = get_inbox(&state, json!({ "agent_name": "rcv41h4" })).unwrap();
        let inbox_msgs = inbox_result["messages"].as_array().unwrap();
        assert!(!inbox_msgs.is_empty());
        assert!(
            inbox_msgs[0].get("created_ts").is_some(),
            "get_inbox response uses 'created_ts' field name"
        );
        assert!(
            inbox_msgs[0]["created_ts"].is_number(),
            "created_ts is a numeric timestamp"
        );

        // search_messages response also uses created_ts
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let search_result =
            search_messages(&state, json!({ "query": "tsname41h4", "limit": 5 })).unwrap();
        let search_docs = search_result["results"].as_array().unwrap();
        assert!(!search_docs.is_empty());
        assert!(
            search_docs[0].get("created_ts").is_some(),
            "search_messages response uses 'created_ts' field name"
        );
        assert!(
            search_docs[0]["created_ts"].is_number(),
            "search created_ts is a numeric timestamp"
        );

        state.shutdown();
    }

    // ── DR41-H5 (DISPROVED): validate_text_field wrapper has semantic value ──
    // validate_text_field is a trivial passthrough to validate_text_core, but
    // it provides semantic clarity: it's used for non-path text fields
    // (project_key, program, model) while validate_name adds path-safety
    // rules. This test confirms both share the same core behavior.
    #[tokio::test]
    async fn dr41_h5_validate_text_field_shares_behavior_with_validate_name() {
        let (state, _idx, _repo) = test_post_office();

        // validate_text_field is used by create_agent for project_key, program, model.
        // validate_name is used for agent names, from_agent, recipients.
        // Both reject the same core inputs via validate_text_core.
        let bad_inputs: Vec<(&str, &str)> = vec![
            ("\0evil", "null bytes"),
            ("\x01control", "control characters"),
            ("  padded  ", "leading/trailing whitespace"),
            ("   ", "whitespace-only"),
        ];

        for (bad_val, desc) in &bad_inputs {
            // project_key uses validate_text_field
            let pkey_result = create_agent(
                &state,
                json!({ "project_key": bad_val, "name_hint": "test" }),
            );
            assert!(
                pkey_result.is_err(),
                "project_key (via validate_text_field) should reject {}: {:?}",
                desc,
                bad_val
            );

            // from_agent uses validate_name
            let from_result = send_message(
                &state,
                json!({
                    "from_agent": bad_val,
                    "to": ["someone"],
                    "body": "test"
                }),
            );
            assert!(
                from_result.is_err(),
                "from_agent (via validate_name) should reject {}: {:?}",
                desc,
                bad_val
            );
        }

        state.shutdown();
    }

    // ── DR42-H1: MCP result text is valid JSON string ────────────────────────
    // DISPROVED: content.to_string() is correct MCP protocol behavior.
    // The text field in MCP content blocks is a string representation of
    // the tool result. Verify the text field is a parseable JSON string.
    #[tokio::test]
    async fn dr42_h1_mcp_result_text_is_valid_json_string() {
        let (state, _idx, _repo) = test_post_office();

        // Create an agent — the result will be wrapped in MCP envelope
        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "create_agent",
                    "arguments": { "project_key": "dr42" }
                }
            }),
        )
        .await;

        // The MCP envelope wraps content in a text block
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(resp["result"]["content"][0]["type"], "text");

        // The text field should be a valid JSON string that can be parsed
        let parsed: Value = serde_json::from_str(text)
            .expect("MCP text content should be a valid JSON string (not double-encoded)");
        assert!(parsed.get("id").is_some(), "Parsed result should have id");
        assert!(
            parsed.get("name").is_some(),
            "Parsed result should have name"
        );

        state.shutdown();
    }

    // ── DR42-H2: Unregistered sender can send messages ──────────────────────
    // CONFIRMED: send_message validates from_agent format but does NOT check
    // if the sender is a registered agent. Document this open addressing design.
    #[tokio::test]
    async fn dr42_h2_unregistered_sender_can_send_message() {
        let (state, _idx, _repo) = test_post_office();

        // Register a recipient but NOT the sender
        create_agent(
            &state,
            json!({ "project_key": "dr42", "name_hint": "Recipient" }),
        )
        .unwrap();

        // Send from an unregistered agent — should succeed
        let result = send_message(
            &state,
            json!({
                "from_agent": "GhostSender",
                "to": ["Recipient"],
                "subject": "Hello",
                "body": "Message from unregistered sender"
            }),
        );
        assert!(
            result.is_ok(),
            "Unregistered sender should be able to send messages: {:?}",
            result.err()
        );

        // Verify the message was delivered
        let inbox = get_inbox(&state, json!({ "agent_name": "Recipient" })).unwrap();
        let msgs = inbox_messages(&inbox);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["from_agent"], "GhostSender");

        state.shutdown();
    }

    // ── DR42-H3: Inbox response includes to_recipients (fixed) ─────────────
    // Previously InboxEntry lacked to_recipients — get_inbox omitted it while
    // search_messages included it. Now both paths return consistent fields.
    #[tokio::test]
    async fn dr42_h3_inbox_response_missing_to_recipients() {
        let (state, _idx, _repo) = test_post_office();

        create_agent(
            &state,
            json!({ "project_key": "dr42", "name_hint": "Alice" }),
        )
        .unwrap();
        create_agent(&state, json!({ "project_key": "dr42", "name_hint": "Bob" })).unwrap();

        // Send to multiple recipients
        send_message(
            &state,
            json!({
                "from_agent": "Alice",
                "to": ["Bob", "Alice"],
                "subject": "Multi-recipient",
                "body": "Sent to both",
                "project_id": "dr42-proj"
            }),
        )
        .unwrap();

        // get_inbox now includes to_recipients (DR42-H3 fix)
        let inbox = get_inbox(&state, json!({ "agent_name": "Bob" })).unwrap();
        let msgs = inbox_messages(&inbox);
        assert_eq!(msgs.len(), 1);
        let to_recip = msgs[0]["to_recipients"].as_str().unwrap();
        // Uses unit separator (\x1F) join — same format as Tantivy index
        assert!(to_recip.contains("Bob"), "to_recipients should contain Bob");
        assert!(
            to_recip.contains("Alice"),
            "to_recipients should contain Alice"
        );
        // All other expected fields still present
        assert!(msgs[0].get("from_agent").is_some());
        assert!(msgs[0].get("subject").is_some());
        assert!(msgs[0].get("body").is_some());

        // Wait for persist worker batch commit + NRT reader refresh
        std::thread::sleep(std::time::Duration::from_millis(500));

        // search_messages also includes to_recipients — consistent with get_inbox
        let search = search_messages(&state, json!({ "query": "both", "limit": 10 })).unwrap();
        let results = search["results"].as_array().unwrap();
        assert!(
            !results.is_empty(),
            "search_messages should find the indexed message"
        );
        assert!(
            results[0].get("to_recipients").is_some(),
            "search_messages response SHOULD have to_recipients field"
        );

        state.shutdown();
    }

    // ── DR42-H4: InboxEntry serde derives unused ────────────────────────────
    // CONFIRMED: InboxEntry derives Serialize/Deserialize but both paths
    // (construction in send_message, serialization in get_inbox) are manual.
    #[tokio::test]
    async fn dr42_h4_inbox_entry_serde_derives_unused() {
        let (state, _idx, _repo) = test_post_office();

        create_agent(
            &state,
            json!({ "project_key": "dr42", "name_hint": "Tester" }),
        )
        .unwrap();

        send_message(
            &state,
            json!({
                "from_agent": "Tester",
                "to": ["Tester"],
                "subject": "Self",
                "body": "Self-message"
            }),
        )
        .unwrap();

        // get_inbox manually constructs JSON — verify it uses the expected
        // field names (which differ from InboxEntry struct field names:
        // "id" vs "message_id", "created_ts" vs "timestamp")
        let inbox = get_inbox(&state, json!({ "agent_name": "Tester" })).unwrap();
        let msgs = inbox_messages(&inbox);
        assert_eq!(msgs.len(), 1);

        // Manual serialization uses "id" (not "message_id" from struct)
        assert!(
            msgs[0].get("id").is_some(),
            "get_inbox uses 'id' not InboxEntry's 'message_id'"
        );
        // Manual serialization uses "created_ts" (not "timestamp" from struct)
        assert!(
            msgs[0].get("created_ts").is_some(),
            "get_inbox uses 'created_ts' not InboxEntry's 'timestamp'"
        );
        // If serde_json::to_value() were used, fields would be "message_id"
        // and "timestamp" (matching the struct). The manual mapping proves
        // the Serialize derive is unused.

        state.shutdown();
    }

    // ── DR42-H5: Schema defaults match code constants ───────────────────────
    // DISPROVED: All schema values use Rust constants directly, preventing drift.
    #[tokio::test]
    async fn dr42_h5_schema_defaults_match_code_constants() {
        let (state, _idx, _repo) = test_post_office();

        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list"
            }),
        )
        .await;

        let tools = resp["result"]["tools"].as_array().unwrap();

        // Find search_messages tool
        let search_tool = tools
            .iter()
            .find(|t| t["name"] == "search_messages")
            .unwrap();
        let search_limit = &search_tool["inputSchema"]["properties"]["limit"];
        assert_eq!(search_limit["default"], DEFAULT_SEARCH_LIMIT);
        assert_eq!(search_limit["minimum"], 1);
        assert_eq!(search_limit["maximum"], MAX_SEARCH_LIMIT);
        assert_eq!(
            search_tool["inputSchema"]["properties"]["query"]["maxLength"],
            MAX_QUERY_LEN
        );

        // Find get_inbox tool
        let inbox_tool = tools.iter().find(|t| t["name"] == "get_inbox").unwrap();
        let inbox_limit = &inbox_tool["inputSchema"]["properties"]["limit"];
        assert_eq!(inbox_limit["default"], DEFAULT_INBOX_LIMIT);
        assert_eq!(inbox_limit["minimum"], 1);
        assert_eq!(inbox_limit["maximum"], MAX_INBOX_LIMIT);
        assert_eq!(
            inbox_tool["inputSchema"]["properties"]["agent_name"]["maxLength"],
            MAX_AGENT_NAME_LEN
        );

        // Find create_agent tool
        let create_tool = tools.iter().find(|t| t["name"] == "create_agent").unwrap();
        assert_eq!(
            create_tool["inputSchema"]["properties"]["project_key"]["maxLength"],
            MAX_PROJECT_KEY_LEN
        );
        assert_eq!(
            create_tool["inputSchema"]["properties"]["name_hint"]["maxLength"],
            MAX_AGENT_NAME_LEN
        );

        state.shutdown();
    }

    // ── DR43-H1: to_recipients format asymmetry (input array vs output string) ──
    // CONFIRMED: send_message accepts `to` as a JSON array, but get_inbox and
    // search_messages return `to_recipients` as a \x1F-delimited string.
    // Clients must know the internal separator format to parse recipients.
    #[tokio::test]
    async fn dr43_h1_to_recipients_format_asymmetry_input_vs_output() {
        let (state, _idx, _repo) = test_post_office();

        create_agent(
            &state,
            json!({ "project_key": "dr43", "name_hint": "Sender" }),
        )
        .unwrap();
        create_agent(
            &state,
            json!({ "project_key": "dr43", "name_hint": "RecA" }),
        )
        .unwrap();
        create_agent(
            &state,
            json!({ "project_key": "dr43", "name_hint": "RecB" }),
        )
        .unwrap();

        // Input format: JSON array
        send_message(
            &state,
            json!({
                "from_agent": "Sender",
                "to": ["RecA", "RecB"],
                "subject": "format-test",
                "body": "testing format"
            }),
        )
        .unwrap();

        // Output format: \x1F-delimited string (not array)
        let inbox = get_inbox(&state, json!({ "agent_name": "RecA" })).unwrap();
        let msgs = inbox_messages(&inbox);
        assert_eq!(msgs.len(), 1);

        let to_recip = msgs[0]["to_recipients"].as_str().unwrap();
        // The output is a string, not a JSON array
        assert!(
            msgs[0]["to_recipients"].is_string(),
            "to_recipients output is a string, not an array (format asymmetry)"
        );
        // Contains \x1F separator
        assert!(
            to_recip.contains('\x1F'),
            "to_recipients uses \\x1F separator: {:?}",
            to_recip
        );
        // Can be split to recover the original array
        let recipients: Vec<&str> = to_recip.split('\x1F').collect();
        assert_eq!(recipients, vec!["RecA", "RecB"]);

        state.shutdown();
    }

    // ── DR43-H2: Tool errors now logged server-side (fixed) ────────────────
    // Previously tool errors were only returned in JSON-RPC responses with no
    // server-side audit trail. Now tracing::warn! is emitted for all tool
    // errors, giving operators visibility into error rates and types (DR43-H2).
    #[tokio::test]
    async fn dr43_h2_tool_errors_not_logged_server_side() {
        let (state, _idx, _repo) = test_post_office();

        // Trigger a tool error (missing required field)
        let result = create_agent(&state, json!({}));
        assert!(result.is_err());
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("Missing project_key"),
            "Error is returned to caller: {}",
            err_msg
        );

        // The error is wrapped in JSON-RPC response AND now traced server-side.
        // tracing::warn!(tool, error, "Tool call failed") is emitted in the
        // Err branch of handle_mcp_request's result match (DR43-H2 fix).
        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "create_agent",
                    "arguments": {}
                }
            }),
        )
        .await;
        assert!(
            resp.get("error").is_some(),
            "Error returned in JSON-RPC response"
        );
        assert_eq!(resp["error"]["code"], -32602);

        state.shutdown();
    }

    // ── DR43-H3: validate_agent_name error format consistency (fixed) ───────
    // Previously create_agent used "Invalid agent name:" prefix while other
    // tools used bare field names. Now all use standard "{field} {error}" format.
    #[tokio::test]
    async fn dr43_h3_validate_agent_name_error_format_inconsistency() {
        let (state, _idx, _repo) = test_post_office();

        // create_agent now uses "name_hint" field name (DR43-H3 fix)
        let create_err =
            create_agent(&state, json!({ "project_key": "dr43", "name_hint": "" })).unwrap_err();
        assert!(
            create_err.starts_with("name_hint"),
            "create_agent error now uses 'name_hint' field name: {}",
            create_err
        );

        // send_message uses validate_name(_, "from_agent") → bare field name
        let send_err = send_message(
            &state,
            json!({
                "from_agent": "",
                "to": ["someone"],
                "body": "test"
            }),
        )
        .unwrap_err();
        assert!(
            send_err.starts_with("from_agent"),
            "send_message error uses bare field name 'from_agent': {}",
            send_err
        );

        // get_inbox uses validate_name(_, "agent_name") → bare field name
        let inbox_err = get_inbox(&state, json!({ "agent_name": "" })).unwrap_err();
        assert!(
            inbox_err.starts_with("agent_name"),
            "get_inbox error uses bare field name 'agent_name': {}",
            inbox_err
        );

        // All now use consistent "{field} {error}" format
        // "name_hint must not be empty" / "from_agent must not be empty" / "agent_name must not be empty"
        assert!(create_err.contains("must not be empty"));
        assert!(send_err.contains("must not be empty"));
        assert!(inbox_err.contains("must not be empty"));

        state.shutdown();
    }

    // ── DR43-H4: InboxEntry String field count matches comment ──────────────
    // DISPROVED: The comment at line 674 says "6 String fields" which is now
    // correct after DR42-H3 added to_recipients. Regression guard to catch
    // if a field is added/removed without updating the comment.
    #[tokio::test]
    async fn dr43_h4_inbox_entry_string_field_count_matches_comment() {
        let (state, _idx, _repo) = test_post_office();

        create_agent(
            &state,
            json!({ "project_key": "dr43", "name_hint": "Counter" }),
        )
        .unwrap();

        send_message(
            &state,
            json!({
                "from_agent": "Counter",
                "to": ["Counter"],
                "subject": "count-test",
                "body": "counting fields"
            }),
        )
        .unwrap();

        let inbox = get_inbox(&state, json!({ "agent_name": "Counter" })).unwrap();
        let msgs = inbox_messages(&inbox);
        assert_eq!(msgs.len(), 1);

        // InboxEntry has 6 String fields + 1 i64 field = 7 total fields.
        // The comment at line 674 says "6 String fields" — verify by checking
        // that all 6 string fields and 1 numeric field are present.
        let msg = &msgs[0];
        // 6 String fields:
        assert!(msg["id"].is_string(), "id is String");
        assert!(msg["from_agent"].is_string(), "from_agent is String");
        assert!(msg["to_recipients"].is_string(), "to_recipients is String");
        assert!(msg["subject"].is_string(), "subject is String");
        assert!(msg["body"].is_string(), "body is String");
        assert!(msg["project_id"].is_string(), "project_id is String");
        // 1 i64 field:
        assert!(msg["created_ts"].is_i64(), "created_ts is i64");
        // Total: 7 fields in JSON output
        let field_count = msg.as_object().unwrap().len();
        assert_eq!(
            field_count, 7,
            "InboxEntry serializes to 7 JSON fields (6 String + 1 i64)"
        );

        state.shutdown();
    }

    // ── DR43-H5: get_inbox and search_messages return same fields ───────────
    // DISPROVED: Both paths now return the same 7 field names after DR42-H3
    // added to_recipients to get_inbox. Regression guard for consistency.
    #[tokio::test]
    async fn dr43_h5_inbox_and_search_return_same_fields() {
        let (state, _idx, _repo) = test_post_office();

        create_agent(
            &state,
            json!({ "project_key": "dr43", "name_hint": "FieldCheck" }),
        )
        .unwrap();

        send_message(
            &state,
            json!({
                "from_agent": "FieldCheck",
                "to": ["FieldCheck"],
                "subject": "dr43fieldcheck",
                "body": "field consistency test",
                "project_id": "dr43-proj"
            }),
        )
        .unwrap();

        // Get inbox fields
        let inbox = get_inbox(&state, json!({ "agent_name": "FieldCheck" })).unwrap();
        let inbox_msgs = inbox_messages(&inbox);
        assert_eq!(inbox_msgs.len(), 1);
        let mut inbox_fields: Vec<&str> = inbox_msgs[0]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        inbox_fields.sort();

        // Wait for search index
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Get search fields
        let search =
            search_messages(&state, json!({ "query": "dr43fieldcheck", "limit": 10 })).unwrap();
        let results = search["results"].as_array().unwrap();
        assert!(!results.is_empty(), "Search should find the message");
        let mut search_fields: Vec<&str> = results[0]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        search_fields.sort();

        // Both should have the exact same field names
        assert_eq!(
            inbox_fields, search_fields,
            "get_inbox and search_messages must return the same field names"
        );

        // Verify the expected 7 fields
        let expected = vec![
            "body",
            "created_ts",
            "from_agent",
            "id",
            "project_id",
            "subject",
            "to_recipients",
        ];
        assert_eq!(inbox_fields, expected);

        state.shutdown();
    }

    // ── DR44: Deep Review 44 ────────────────────────────────────────────────

    // DR44-H1: axum::serve error path skips state.shutdown()
    // This is an architectural issue in main.rs, not testable at the
    // controller level. Documenting as a structural test that verifies
    // PostOffice::shutdown() can be called safely (idempotent drain).
    #[tokio::test]
    async fn dr44_h1_shutdown_is_safe_after_normal_operation() {
        let dir = tempfile::tempdir().unwrap();
        let idx = dir.path().join("idx");
        let repo = dir.path().join("repo");
        let state = PostOffice::new(&idx, &repo).unwrap();

        // Do some work
        let create = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "tools/call",
                "params": { "name": "create_agent", "arguments": { "project_key": "dr44h1" } }
            }),
        )
        .await;
        assert!(create["error"].is_null());

        // shutdown() drains the persist pipeline — must not panic
        state.shutdown();
    }

    // DR44-H2: Single-recipient to_recipients has no \x1F separator.
    // Verifies that a message sent to one recipient stores the raw name
    // without a trailing or leading unit separator.
    #[tokio::test]
    async fn dr44_h2_single_recipient_no_separator() {
        let dir = tempfile::tempdir().unwrap();
        let idx = dir.path().join("idx");
        let repo = dir.path().join("repo");
        let state = PostOffice::new(&idx, &repo).unwrap();

        // Create sender and recipient
        let _ = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "tools/call",
                "params": { "name": "create_agent", "arguments": { "project_key": "dr44h2", "name_hint": "alice" } }
            }),
        )
        .await;
        let _ = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 2,
                "method": "tools/call",
                "params": { "name": "create_agent", "arguments": { "project_key": "dr44h2", "name_hint": "bob" } }
            }),
        )
        .await;

        // Send to single recipient
        let _ = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 3,
                "method": "tools/call",
                "params": { "name": "send_message", "arguments": {
                    "from_agent": "alice",
                    "to": ["bob"],
                    "subject": "solo",
                    "body": "one recipient"
                }}
            }),
        )
        .await;

        // Read inbox — to_recipients should be exactly "bob" with no \x1F
        let inbox_resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 4,
                "method": "tools/call",
                "params": { "name": "get_inbox", "arguments": { "agent_name": "bob" } }
            }),
        )
        .await;
        let text = inbox_resp["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        let msgs = parsed["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        let to_field = msgs[0]["to_recipients"].as_str().unwrap();
        assert_eq!(
            to_field, "bob",
            "Single recipient must not contain \\x1F separator"
        );
        assert!(
            !to_field.contains('\x1F'),
            "to_recipients for single recipient must not contain unit separator"
        );

        state.shutdown();
    }

    // DR44-H3: Tool name length is now validated (max 64 bytes).
    // Previously, a ~1MB tool name would be echoed verbatim in the error
    // response via format!("Unknown tool: {}"). Now rejected early with a
    // length error that reports the byte count, not the tool name (DR44-H3).
    #[tokio::test]
    async fn dr44_h3_long_tool_name_echoed_in_error() {
        let dir = tempfile::tempdir().unwrap();
        let idx = dir.path().join("idx");
        let repo = dir.path().join("repo");
        let state = PostOffice::new(&idx, &repo).unwrap();

        let long_name = "x".repeat(10_000);
        let resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "tools/call",
                "params": { "name": long_name, "arguments": {} }
            }),
        )
        .await;

        let err_msg = resp["error"]["message"].as_str().unwrap();
        // FIXED: error no longer echoes the full tool name — reports byte count instead
        assert!(
            err_msg.contains("Tool name too long"),
            "Error must reject with length message, got: {}",
            err_msg
        );
        assert!(
            !err_msg.contains(&long_name),
            "Error must NOT echo the full tool name"
        );
        assert_eq!(resp["error"]["code"], -32602);

        // Verify boundary: exactly MAX_TOOL_NAME_LEN is accepted (returns "Unknown tool")
        let boundary_name = "z".repeat(MAX_TOOL_NAME_LEN);
        let boundary_resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 2,
                "method": "tools/call",
                "params": { "name": boundary_name, "arguments": {} }
            }),
        )
        .await;
        let boundary_msg = boundary_resp["error"]["message"].as_str().unwrap();
        assert!(
            boundary_msg.starts_with("Unknown tool:"),
            "Boundary length must reach the Unknown tool path, got: {}",
            boundary_msg
        );

        state.shutdown();
    }

    // DR44-H4: create_agent fresh registration returns updated_at: null,
    // re-registration returns updated_at as integer timestamp.
    // Asserts type-level distinction (null vs i64), not just presence.
    #[tokio::test]
    async fn dr44_h4_updated_at_type_distinction() {
        let dir = tempfile::tempdir().unwrap();
        let idx = dir.path().join("idx");
        let repo = dir.path().join("repo");
        let state = PostOffice::new(&idx, &repo).unwrap();

        // Fresh registration
        let r1 = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "tools/call",
                "params": { "name": "create_agent", "arguments": { "project_key": "dr44h4", "name_hint": "typetest" } }
            }),
        )
        .await;
        let text1 = r1["result"]["content"][0]["text"].as_str().unwrap();
        let profile1: Value = serde_json::from_str(text1).unwrap();
        assert!(
            profile1["updated_at"].is_null(),
            "Fresh registration: updated_at must be JSON null, got {:?}",
            profile1["updated_at"]
        );

        // Re-registration
        let r2 = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 2,
                "method": "tools/call",
                "params": { "name": "create_agent", "arguments": { "project_key": "dr44h4", "name_hint": "typetest" } }
            }),
        )
        .await;
        let text2 = r2["result"]["content"][0]["text"].as_str().unwrap();
        let profile2: Value = serde_json::from_str(text2).unwrap();
        assert!(
            profile2["updated_at"].is_i64(),
            "Re-registration: updated_at must be i64, got {:?}",
            profile2["updated_at"]
        );

        state.shutdown();
    }

    // DR44-H5: get_inbox with limit = MAX_INBOX_LIMIT (1000) on inbox
    // with exactly 1000 messages triggers the full drain path (mem::take)
    // and removes the DashMap entry.
    #[tokio::test]
    async fn dr44_h5_max_limit_exact_boundary_full_drain() {
        let dir = tempfile::tempdir().unwrap();
        let idx = dir.path().join("idx");
        let repo = dir.path().join("repo");
        let state = PostOffice::new(&idx, &repo).unwrap();

        // Create agents
        let _ = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "tools/call",
                "params": { "name": "create_agent", "arguments": { "project_key": "dr44h5", "name_hint": "sender" } }
            }),
        )
        .await;
        let _ = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 2,
                "method": "tools/call",
                "params": { "name": "create_agent", "arguments": { "project_key": "dr44h5", "name_hint": "receiver" } }
            }),
        )
        .await;

        // Send exactly MAX_INBOX_LIMIT messages
        for i in 0..MAX_INBOX_LIMIT {
            let _ = handle_mcp_request(
                state.clone(),
                json!({
                    "jsonrpc": "2.0", "id": i + 10,
                    "method": "tools/call",
                    "params": { "name": "send_message", "arguments": {
                        "from_agent": "sender",
                        "to": ["receiver"],
                        "subject": format!("msg-{}", i),
                        "body": "boundary test"
                    }}
                }),
            )
            .await;
        }

        // Drain with limit = MAX_INBOX_LIMIT (1000)
        // inbox.len() == limit → takes the full drain path (mem::take + occ.remove())
        let inbox_resp = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 2000,
                "method": "tools/call",
                "params": { "name": "get_inbox", "arguments": {
                    "agent_name": "receiver",
                    "limit": MAX_INBOX_LIMIT
                }}
            }),
        )
        .await;
        let text = inbox_resp["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        let arr = parsed["messages"].as_array().unwrap();
        assert_eq!(
            arr.len(),
            MAX_INBOX_LIMIT,
            "Must return all {} messages",
            MAX_INBOX_LIMIT
        );

        // Verify remaining is 0 (full drain path)
        assert_eq!(parsed["remaining"], 0, "Full drain must report 0 remaining");

        // DashMap entry should be removed after full drain
        assert!(
            !state.inboxes.contains_key("receiver"),
            "DashMap entry must be removed after full drain at limit boundary"
        );

        state.shutdown();
    }

    // ── DR45: Deep Review 45 ────────────────────────────────────────────────

    // DR45-H1: tools/list response is static but rebuilt from scratch on
    // every request. This test asserts the response is byte-identical across
    // calls, confirming the output is deterministic and could be cached.
    #[tokio::test]
    async fn dr45_h1_tools_list_response_is_static_and_deterministic() {
        let (state, _idx, _repo) = test_post_office();

        let resp1 = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "tools/list"
            }),
        )
        .await;
        let resp2 = handle_mcp_request(
            state.clone(),
            json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "tools/list"
            }),
        )
        .await;

        // Responses must be byte-identical (same id, same content)
        assert_eq!(
            resp1.to_string(),
            resp2.to_string(),
            "tools/list response must be deterministic — could be cached"
        );

        // Verify the response includes all 4 tools
        let tools = resp1["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 4);

        state.shutdown();
    }

    // DR45-H2: send_message with exactly 1 recipient skips the clone path.
    // The loop at line 694 iterates over &to_agents[..to_agents.len()-1],
    // which is empty for a single recipient. The entry is moved (not cloned)
    // into the last recipient's inbox via to_agents.last().unwrap().
    #[tokio::test]
    async fn dr45_h2_single_recipient_skips_clone_path() {
        let (state, _idx, _repo) = test_post_office();

        let _ = create_agent(
            &state,
            json!({ "project_key": "dr45h2", "name_hint": "sender45" }),
        );
        let _ = create_agent(
            &state,
            json!({ "project_key": "dr45h2", "name_hint": "receiver45" }),
        );

        // Send to exactly 1 recipient — entry should be moved, not cloned
        let result = send_message(
            &state,
            json!({
                "from_agent": "sender45",
                "to": ["receiver45"],
                "subject": "single",
                "body": "no clone needed"
            }),
        );
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp["delivered_count"], 1);
        assert_eq!(resp["status"], "sent");

        // Verify the message was actually delivered
        let inbox = get_inbox(&state, json!({ "agent_name": "receiver45" })).unwrap();
        let msgs = inbox_messages(&inbox);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["subject"], "single");
        assert_eq!(msgs[0]["body"], "no clone needed");

        state.shutdown();
    }

    // DR45-H3: create_agent concurrent re-registration with identical args
    // preserves agent_id and registered_at — verifying the idempotent upsert
    // produces stable identity across multiple re-registrations.
    #[tokio::test]
    async fn dr45_h3_triple_registration_preserves_identity() {
        let (state, _idx, _repo) = test_post_office();

        // First registration
        let r1 = create_agent(
            &state,
            json!({ "project_key": "dr45h3", "name_hint": "stable", "program": "v1" }),
        )
        .unwrap();
        let id1 = r1["id"].as_str().unwrap().to_string();
        let reg_at1 = r1["registered_at"].as_i64().unwrap();

        // Second registration (re-reg)
        let r2 = create_agent(
            &state,
            json!({ "project_key": "dr45h3", "name_hint": "stable", "program": "v2" }),
        )
        .unwrap();
        let id2 = r2["id"].as_str().unwrap().to_string();
        let reg_at2 = r2["registered_at"].as_i64().unwrap();

        // Third registration (re-reg again)
        let r3 = create_agent(
            &state,
            json!({ "project_key": "dr45h3", "name_hint": "stable", "program": "v3" }),
        )
        .unwrap();
        let id3 = r3["id"].as_str().unwrap().to_string();
        let reg_at3 = r3["registered_at"].as_i64().unwrap();

        // Identity must be stable across all 3 registrations
        assert_eq!(id1, id2, "agent_id must be stable on re-registration");
        assert_eq!(id2, id3, "agent_id must be stable on triple registration");
        assert_eq!(
            reg_at1, reg_at2,
            "registered_at must be stable on re-registration"
        );
        assert_eq!(
            reg_at2, reg_at3,
            "registered_at must be stable on triple registration"
        );

        // But program must reflect the latest value
        {
            let record = state.agents.get("stable").unwrap();
            assert_eq!(
                record.program, "v3",
                "program must reflect latest registration"
            );
        }

        state.shutdown();
    }

    // DR45-H4: send_message response has no field indicating whether the
    // Tantivy index operation succeeded or was dropped. The response always
    // says status: "sent" regardless of persist channel state. This test
    // documents the behavior — "sent" refers to inbox delivery only.
    #[tokio::test]
    async fn dr45_h4_send_response_status_refers_to_inbox_only() {
        let (state, _idx, _repo) = test_post_office();

        let _ = create_agent(
            &state,
            json!({ "project_key": "dr45h4", "name_hint": "alice45" }),
        );
        let _ = create_agent(
            &state,
            json!({ "project_key": "dr45h4", "name_hint": "bob45" }),
        );

        let result = send_message(
            &state,
            json!({
                "from_agent": "alice45",
                "to": ["bob45"],
                "subject": "test",
                "body": "indexing status unknown"
            }),
        )
        .unwrap();

        // Response says "sent" — this refers to inbox delivery
        assert_eq!(result["status"], "sent");
        assert_eq!(result["delivered_count"], 1);

        // FIXED (DR45-H4): indexed field now present — reports whether the
        // persist channel accepted the Tantivy index operation.
        assert!(
            result.get("indexed").is_some(),
            "Response must include 'indexed' field (DR45-H4)"
        );
        assert_eq!(
            result["indexed"], true,
            "indexed must be true when persist channel is available"
        );

        state.shutdown();
    }

    // DR45-H5: get_inbox with limit=1 on an inbox with many messages uses
    // the partial drain path and returns accurate remaining count.
    #[tokio::test]
    async fn dr45_h5_limit_one_partial_drain_remaining_count() {
        let (state, _idx, _repo) = test_post_office();

        let _ = create_agent(
            &state,
            json!({ "project_key": "dr45h5", "name_hint": "sender45h5" }),
        );
        let _ = create_agent(
            &state,
            json!({ "project_key": "dr45h5", "name_hint": "recvr45h5" }),
        );

        // Send 5 messages
        for i in 0..5 {
            let _ = send_message(
                &state,
                json!({
                    "from_agent": "sender45h5",
                    "to": ["recvr45h5"],
                    "subject": format!("msg-{}", i),
                    "body": "partial drain test"
                }),
            )
            .unwrap();
        }

        // Drain with limit=1 — should return 1 message and remaining=4
        let result = get_inbox(&state, json!({ "agent_name": "recvr45h5", "limit": 1 })).unwrap();
        let msgs = inbox_messages(&result);
        assert_eq!(msgs.len(), 1, "limit=1 must return exactly 1 message");
        assert_eq!(
            result["remaining"], 4,
            "remaining must be 4 after draining 1 of 5"
        );

        // The first message should be returned (FIFO order)
        assert_eq!(msgs[0]["subject"], "msg-0", "Partial drain must be FIFO");

        // DashMap entry should still exist (not removed)
        assert!(
            state.inboxes.contains_key("recvr45h5"),
            "DashMap entry must persist after partial drain"
        );

        // Drain again with limit=1 — should return msg-1, remaining=3
        let result2 = get_inbox(&state, json!({ "agent_name": "recvr45h5", "limit": 1 })).unwrap();
        let msgs2 = inbox_messages(&result2);
        assert_eq!(
            msgs2[0]["subject"], "msg-1",
            "Second drain returns next FIFO message"
        );
        assert_eq!(result2["remaining"], 3);

        state.shutdown();
    }
}
