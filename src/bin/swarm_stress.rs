use reqwest::Client;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

const SERVER_URL: &str = "http://localhost:8000/mcp";

// ── Topology ──────────────────────────────────────────────────────────────────
// 10 Divisions × 10 Teams × 20 Agents = 2,000 agents
const DIVISIONS: usize = 10;
const TEAMS_PER_DIV: usize = 10;
const AGENTS_PER_TEAM: usize = 20;
const TOTAL_AGENTS: usize = DIVISIONS * TEAMS_PER_DIV * AGENTS_PER_TEAM;

// ── Concurrency Tuning ───────────────────────────────────────────────────────
const REGISTRATION_CONCURRENCY: usize = 500; // parallel registration batch size
const MSG_WAVES: usize = 3; // number of messaging waves
const SEARCH_QUERIES: usize = 2000; // total search requests
const SEARCH_CONCURRENCY: usize = 200; // parallel search batch size
const MSG_CONCURRENCY: usize = 500; // parallel message sends (DashMap is lock-free)

// Derived the same way the server does: Uuid::new_v5(NAMESPACE_DNS, key)
fn project_id_for_key(key: &str) -> String {
    format!(
        "proj_{}",
        Uuid::new_v5(&Uuid::NAMESPACE_DNS, key.as_bytes()).simple()
    )
}

const PROJECT_KEY: &str = "swarm_stress";

// ── Counters ─────────────────────────────────────────────────────────────────
struct Metrics {
    registrations_ok: AtomicU64,
    registrations_err: AtomicU64,
    messages_ok: AtomicU64,
    messages_err: AtomicU64,
    inbox_ok: AtomicU64,
    inbox_err: AtomicU64,
    inbox_msgs_received: AtomicU64,
    search_ok: AtomicU64,
    search_err: AtomicU64,
    search_hits: AtomicU64,
}

impl Metrics {
    fn new() -> Self {
        Self {
            registrations_ok: AtomicU64::new(0),
            registrations_err: AtomicU64::new(0),
            messages_ok: AtomicU64::new(0),
            messages_err: AtomicU64::new(0),
            inbox_ok: AtomicU64::new(0),
            inbox_err: AtomicU64::new(0),
            inbox_msgs_received: AtomicU64::new(0),
            search_ok: AtomicU64::new(0),
            search_err: AtomicU64::new(0),
            search_hits: AtomicU64::new(0),
        }
    }
}

fn agent_name(div: usize, team: usize, idx: usize) -> String {
    format!("D{:02}_T{:02}_A{:03}", div, team, idx)
}

fn team_lead(div: usize, team: usize) -> String {
    agent_name(div, team, 0)
}

fn division_head(div: usize) -> String {
    agent_name(div, 0, 0)
}

// ── Helpers ──────────────────────────────────────────────────────────────────
async fn mcp_call(client: &Client, tool: &str, args: Value) -> Result<Value, String> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": args }
    });
    let resp = client
        .post(SERVER_URL)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("HTTP: {}", e))?;
    let body: Value = resp.json().await.map_err(|e| format!("JSON: {}", e))?;
    if body.get("error").is_some() {
        Err(format!("MCP: {}", body["error"]["message"]))
    } else {
        Ok(body)
    }
}

fn parse_content_text(resp: &Value) -> Option<String> {
    resp.get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.get(0))
        .and_then(|t| t.get("text"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
}

// ── Phases ───────────────────────────────────────────────────────────────────

/// Phase 1: Register all 2,000 agents with bounded concurrency.
async fn phase_register(client: &Client, metrics: &Arc<Metrics>) -> f64 {
    println!("\n{}", "=".repeat(60));
    println!("  PHASE 1: MASS REGISTRATION ({} agents)", TOTAL_AGENTS);
    println!(
        "  Concurrency: {} parallel requests",
        REGISTRATION_CONCURRENCY
    );
    println!("{}", "=".repeat(60));

    let sem = Arc::new(tokio::sync::Semaphore::new(REGISTRATION_CONCURRENCY));
    let start = Instant::now();
    let mut handles = Vec::with_capacity(TOTAL_AGENTS);

    for div in 0..DIVISIONS {
        for team in 0..TEAMS_PER_DIV {
            for idx in 0..AGENTS_PER_TEAM {
                let client = client.clone();
                let metrics = metrics.clone();
                let sem = sem.clone();
                let name = agent_name(div, team, idx);
                let role = match idx {
                    0 => format!("Team Lead, Division {}, Team {}", div, team),
                    _ => format!("Agent, Division {}, Team {}", div, team),
                };

                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    let result = mcp_call(
                        &client,
                        "create_agent",
                        json!({
                            "project_key": "swarm_stress",
                            "name_hint": name,
                            "program": "swarm_stress",
                            "model": "synthetic",
                            "task_description": role
                        }),
                    )
                    .await;
                    match result {
                        Ok(_) => metrics.registrations_ok.fetch_add(1, Ordering::Relaxed),
                        Err(e) => {
                            // Duplicate agent names (same project_id+name) are expected on re-run
                            if !e.contains("UNIQUE") {
                                eprintln!("  REG ERR: {}", e);
                            }
                            metrics.registrations_err.fetch_add(1, Ordering::Relaxed)
                        }
                    };
                }));
            }
        }
    }
    for h in handles {
        let _ = h.await;
    }

    let elapsed = start.elapsed();
    let ok = metrics.registrations_ok.load(Ordering::Relaxed);
    let err = metrics.registrations_err.load(Ordering::Relaxed);
    let rate = ok as f64 / elapsed.as_secs_f64();
    println!(
        "  Done: {} ok, {} err in {:.2?}  ({:.0} registrations/s)",
        ok, err, elapsed, rate
    );
    rate
}

/// Phase 2: Intra-team chain relay + team-lead broadcasts + cross-div coordination.
/// Each wave fires messages in parallel across all teams.
async fn phase_messaging(client: &Client, metrics: &Arc<Metrics>, project_id: &str) -> (f64, u64) {
    println!("\n{}", "=".repeat(60));
    println!("  PHASE 2: MULTI-PATTERN MESSAGING ({} waves)", MSG_WAVES);
    println!("  Patterns: chain-relay, team-broadcast, cross-division");
    println!("  Concurrency: {} parallel sends", MSG_CONCURRENCY);
    println!("{}", "=".repeat(60));

    let sem = Arc::new(tokio::sync::Semaphore::new(MSG_CONCURRENCY));
    let first_errors = Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    let mut total_sent: u64 = 0;

    for wave in 0..MSG_WAVES {
        println!("  Wave {}/{}...", wave + 1, MSG_WAVES);
        let mut handles = Vec::new();

        for div in 0..DIVISIONS {
            for team in 0..TEAMS_PER_DIV {
                // Pattern A: Chain relay (agent i → agent i+1 within team)
                for idx in 0..(AGENTS_PER_TEAM - 1) {
                    let client = client.clone();
                    let metrics = metrics.clone();
                    let from = agent_name(div, team, idx);
                    let to = agent_name(div, team, idx + 1);
                    let subject = format!("W{} chain D{}T{} {}->{}", wave, div, team, idx, idx + 1);
                    let pid = project_id.to_string();
                    let sem = sem.clone();
                    let first_errors = first_errors.clone();
                    handles.push(tokio::spawn(async move {
                        let _permit = sem.acquire().await.unwrap();
                        let r = mcp_call(
                            &client,
                            "send_message",
                            json!({
                                "from_agent": from,
                                "to": [to],
                                "subject": subject,
                                "body": format!(
                                    "Chain relay wave {}. Passing intel downstream. Division {} Team {} Agent {}.",
                                    wave, div, team, idx
                                ),
                                "project_id": pid
                            }),
                        )
                        .await;
                        match r {
                            Ok(_) => metrics.messages_ok.fetch_add(1, Ordering::Relaxed),
                            Err(e) => {
                                if first_errors.fetch_add(1, Ordering::Relaxed) < 3 {
                                    eprintln!("  MSG ERR: {}", e);
                                }
                                metrics.messages_err.fetch_add(1, Ordering::Relaxed)
                            }
                        };
                    }));
                }

                // Pattern B: Team lead broadcasts to all members (1-to-many)
                {
                    let client = client.clone();
                    let metrics = metrics.clone();
                    let lead = team_lead(div, team);
                    let recipients: Vec<String> = (1..AGENTS_PER_TEAM)
                        .map(|i| agent_name(div, team, i))
                        .collect();
                    let subject = format!("W{} broadcast D{}T{}", wave, div, team);
                    let pid = project_id.to_string();
                    let sem = sem.clone();
                    let first_errors = first_errors.clone();
                    handles.push(tokio::spawn(async move {
                        let _permit = sem.acquire().await.unwrap();
                        let r = mcp_call(
                            &client,
                            "send_message",
                            json!({
                                "from_agent": lead,
                                "to": recipients,
                                "subject": subject,
                                "body": format!(
                                    "DIRECTIVE: Wave {} status report. All hands acknowledge. Division {} Team {}.",
                                    wave, div, team
                                ),
                                "project_id": pid
                            }),
                        )
                        .await;
                        match r {
                            Ok(_) => metrics.messages_ok.fetch_add(1, Ordering::Relaxed),
                            Err(e) => {
                                if first_errors.fetch_add(1, Ordering::Relaxed) < 3 {
                                    eprintln!("  MSG ERR: {}", e);
                                }
                                metrics.messages_err.fetch_add(1, Ordering::Relaxed)
                            }
                        };
                    }));
                }
            }

            // Pattern C: Division head → all other division heads (cross-div coordination)
            {
                let client = client.clone();
                let metrics = metrics.clone();
                let from = division_head(div);
                let targets: Vec<String> = (0..DIVISIONS)
                    .filter(|d| *d != div)
                    .map(division_head)
                    .collect();
                let subject = format!("W{} cross-div from D{}", wave, div);
                let pid = project_id.to_string();
                let sem = sem.clone();
                let first_errors = first_errors.clone();
                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    let r = mcp_call(
                        &client,
                        "send_message",
                        json!({
                            "from_agent": from,
                            "to": targets,
                            "subject": subject,
                            "body": format!(
                                "Cross-division sync wave {}. Division {} reporting nominal.",
                                wave, div
                            ),
                            "project_id": pid
                        }),
                    )
                    .await;
                    match r {
                        Ok(_) => metrics.messages_ok.fetch_add(1, Ordering::Relaxed),
                        Err(e) => {
                            if first_errors.fetch_add(1, Ordering::Relaxed) < 3 {
                                eprintln!("  MSG ERR: {}", e);
                            }
                            metrics.messages_err.fetch_add(1, Ordering::Relaxed)
                        }
                    };
                }));
            }
        }

        let wave_count = handles.len() as u64;
        total_sent += wave_count;
        for h in handles {
            let _ = h.await;
        }
    }

    let elapsed = start.elapsed();
    let ok = metrics.messages_ok.load(Ordering::Relaxed);
    let err = metrics.messages_err.load(Ordering::Relaxed);
    let rate = ok as f64 / elapsed.as_secs_f64();
    println!(
        "  Done: {} ok, {} err in {:.2?}  ({:.0} msgs/s)",
        ok, err, elapsed, rate
    );
    (rate, total_sent)
}

/// Phase 3: Every agent checks inbox concurrently.
async fn phase_inbox_drain(client: &Client, metrics: &Arc<Metrics>) -> f64 {
    println!("\n{}", "=".repeat(60));
    println!(
        "  PHASE 3: INBOX DRAIN ({} agents check mail)",
        TOTAL_AGENTS
    );
    println!("{}", "=".repeat(60));

    let sem = Arc::new(tokio::sync::Semaphore::new(REGISTRATION_CONCURRENCY));
    let start = Instant::now();
    let mut handles = Vec::with_capacity(TOTAL_AGENTS);

    for div in 0..DIVISIONS {
        for team in 0..TEAMS_PER_DIV {
            for idx in 0..AGENTS_PER_TEAM {
                let client = client.clone();
                let metrics = metrics.clone();
                let sem = sem.clone();
                let name = agent_name(div, team, idx);

                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    let result =
                        mcp_call(&client, "get_inbox", json!({ "agent_name": name })).await;
                    match result {
                        Ok(resp) => {
                            metrics.inbox_ok.fetch_add(1, Ordering::Relaxed);
                            if let Some(text) = parse_content_text(&resp) {
                                if let Ok(msgs) = serde_json::from_str::<Vec<Value>>(&text) {
                                    metrics
                                        .inbox_msgs_received
                                        .fetch_add(msgs.len() as u64, Ordering::Relaxed);
                                }
                            }
                        }
                        Err(_) => {
                            metrics.inbox_err.fetch_add(1, Ordering::Relaxed);
                        }
                    };
                }));
            }
        }
    }
    for h in handles {
        let _ = h.await;
    }

    let elapsed = start.elapsed();
    let ok = metrics.inbox_ok.load(Ordering::Relaxed);
    let err = metrics.inbox_err.load(Ordering::Relaxed);
    let received = metrics.inbox_msgs_received.load(Ordering::Relaxed);
    let rate = ok as f64 / elapsed.as_secs_f64();
    println!(
        "  Done: {} ok, {} err in {:.2?}  ({:.0} inbox/s, {} total msgs received)",
        ok, err, elapsed, rate, received
    );
    rate
}

/// Phase 4: Concurrent full-text search stress.
async fn phase_search_stress(client: &Client, metrics: &Arc<Metrics>) -> f64 {
    // Allow Tantivy to commit + refresh
    println!("\n  Waiting 2s for Tantivy commit cycle...");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    println!("{}", "=".repeat(60));
    println!(
        "  PHASE 4: SEARCH STRESS ({} queries, {} concurrent)",
        SEARCH_QUERIES, SEARCH_CONCURRENCY
    );
    println!("{}", "=".repeat(60));

    let queries = [
        "chain relay intel",
        "DIRECTIVE status report",
        "cross-division sync",
        "reporting nominal",
        "downstream agent",
        "wave acknowledge",
        "division team",
        "intel passing",
        "broadcast hands",
        "coordination sync",
    ];

    let sem = Arc::new(tokio::sync::Semaphore::new(SEARCH_CONCURRENCY));
    let start = Instant::now();
    let mut handles = Vec::with_capacity(SEARCH_QUERIES);

    for i in 0..SEARCH_QUERIES {
        let client = client.clone();
        let metrics = metrics.clone();
        let sem = sem.clone();
        let query = queries[i % queries.len()].to_string();

        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let result = mcp_call(
                &client,
                "search_messages",
                json!({ "query": query, "limit": 10 }),
            )
            .await;
            match result {
                Ok(resp) => {
                    metrics.search_ok.fetch_add(1, Ordering::Relaxed);
                    if let Some(text) = parse_content_text(&resp) {
                        if let Ok(hits) = serde_json::from_str::<Vec<Value>>(&text) {
                            metrics
                                .search_hits
                                .fetch_add(hits.len() as u64, Ordering::Relaxed);
                        }
                    }
                }
                Err(_) => {
                    metrics.search_err.fetch_add(1, Ordering::Relaxed);
                }
            };
        }));
    }
    for h in handles {
        let _ = h.await;
    }

    let elapsed = start.elapsed();
    let ok = metrics.search_ok.load(Ordering::Relaxed);
    let err = metrics.search_err.load(Ordering::Relaxed);
    let hits = metrics.search_hits.load(Ordering::Relaxed);
    let rate = ok as f64 / elapsed.as_secs_f64();
    println!(
        "  Done: {} ok, {} err in {:.2?}  ({:.0} q/s, {} total hits)",
        ok, err, elapsed, rate, hits
    );
    rate
}

// ── Main ─────────────────────────────────────────────────────────────────────
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!(
        "║       ALLMAN SWARM STRESS TEST — {} AGENTS              ║",
        TOTAL_AGENTS
    );
    println!(
        "║  Topology: {} div x {} teams x {} agents/team            ║",
        DIVISIONS, TEAMS_PER_DIV, AGENTS_PER_TEAM
    );
    println!("║  Target:   {}                            ║", SERVER_URL);
    println!("╚══════════════════════════════════════════════════════════════╝");

    // Build a client with high connection pool
    let client = Client::builder()
        .pool_max_idle_per_host(500)
        .pool_idle_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    // Health check
    let health = client.get("http://localhost:8000/health").send().await;
    match health {
        Ok(r) if r.status().is_success() => println!("\nServer health: OK"),
        _ => {
            eprintln!("\nERROR: Server not reachable at localhost:8000. Aborting.");
            std::process::exit(1);
        }
    }

    let metrics = Arc::new(Metrics::new());
    let global_start = Instant::now();

    // ── Execute Phases ───────────────────────────────────────────────────────
    let project_id = project_id_for_key(PROJECT_KEY);
    println!("  Project ID: {}", project_id);

    let reg_rate = phase_register(&client, &metrics).await;
    let (msg_rate, msg_count) = phase_messaging(&client, &metrics, &project_id).await;
    let inbox_rate = phase_inbox_drain(&client, &metrics).await;
    let search_rate = phase_search_stress(&client, &metrics).await;

    let total_elapsed = global_start.elapsed();

    // ── Final Report ─────────────────────────────────────────────────────────
    let reg_ok = metrics.registrations_ok.load(Ordering::Relaxed);
    let reg_err = metrics.registrations_err.load(Ordering::Relaxed);
    let msg_ok = metrics.messages_ok.load(Ordering::Relaxed);
    let msg_err = metrics.messages_err.load(Ordering::Relaxed);
    let inb_ok = metrics.inbox_ok.load(Ordering::Relaxed);
    let inb_err = metrics.inbox_err.load(Ordering::Relaxed);
    let inb_msgs = metrics.inbox_msgs_received.load(Ordering::Relaxed);
    let srch_ok = metrics.search_ok.load(Ordering::Relaxed);
    let srch_err = metrics.search_err.load(Ordering::Relaxed);
    let srch_hits = metrics.search_hits.load(Ordering::Relaxed);

    let total_requests =
        reg_ok + reg_err + msg_ok + msg_err + inb_ok + inb_err + srch_ok + srch_err;

    println!("\n╔══════════════════════════════════════════════════════════════╗");
    println!("║                    FINAL RESULTS                           ║");
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!(
        "║  Total Time:       {:>10.2?}                              ║",
        total_elapsed
    );
    println!(
        "║  Total Requests:   {:>10}                              ║",
        total_requests
    );
    println!(
        "║  Aggregate Rate:   {:>10.0} req/s                      ║",
        total_requests as f64 / total_elapsed.as_secs_f64()
    );
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!("║  Phase          │  OK     │ Err   │  Rate                  ║");
    println!(
        "║  Registration   │ {:>6}  │ {:>5} │ {:>8.0} reg/s           ║",
        reg_ok, reg_err, reg_rate
    );
    println!(
        "║  Messaging      │ {:>6}  │ {:>5} │ {:>8.0} msg/s           ║",
        msg_ok, msg_err, msg_rate
    );
    println!(
        "║  Inbox Drain    │ {:>6}  │ {:>5} │ {:>8.0} inbox/s         ║",
        inb_ok, inb_err, inbox_rate
    );
    println!(
        "║  Search         │ {:>6}  │ {:>5} │ {:>8.0} q/s             ║",
        srch_ok, srch_err, search_rate
    );
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!(
        "║  Messages Sent (calls):    {:>10}                      ║",
        msg_count
    );
    println!(
        "║  Inbox Msgs Received:      {:>10}                      ║",
        inb_msgs
    );
    println!(
        "║  Search Hits Returned:     {:>10}                      ║",
        srch_hits
    );
    println!("╚══════════════════════════════════════════════════════════════╝");

    // Error rate warning
    let total_err = reg_err + msg_err + inb_err + srch_err;
    if total_err > 0 {
        let err_pct = total_err as f64 / total_requests as f64 * 100.0;
        println!(
            "\n  WARNING: {:.2}% error rate ({} / {} requests failed)",
            err_pct, total_err, total_requests
        );
    } else {
        println!("\n  ZERO errors across all {} requests.", total_requests);
    }

    Ok(())
}
