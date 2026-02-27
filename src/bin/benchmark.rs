use reqwest::Client;
use serde_json::json;
use std::time::Instant;
use std::sync::Arc;
use tokio::sync::Barrier;

const SERVER_URL: &str = "http://localhost:8000/mcp";
const AGENT_COUNT: usize = 100;
const MESSAGES_PER_AGENT: usize = 10;
const TOTAL_REQUESTS: usize = AGENT_COUNT * MESSAGES_PER_AGENT;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("--- 🏎️  Allman Performance Benchmark ---");
    println!("Target: {}", SERVER_URL);
    println!("Agents: {}", AGENT_COUNT);
    println!("Msgs/Agent: {}", MESSAGES_PER_AGENT);
    println!("Total Msgs: {}\n", TOTAL_REQUESTS);

    let client = Client::new();

    // 1. Registration Benchmark
    let start = Instant::now();
    let mut handles = vec![];
    for i in 0..AGENT_COUNT {
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            let name = format!("BenchAgent_{}", i);
            let payload = json!({
                "jsonrpc": "2.0", "id": i, "method": "tools/call",
                "params": {
                    "name": "create_agent",
                    "arguments": { "project_key": "bench", "name_hint": name, "model": "bench-model" }
                }
            });
            let _ = client.post(SERVER_URL).json(&payload).send().await;
        }));
    }
    for h in handles { h.await?; }
    let duration = start.elapsed();
    println!("✅ Registration: {:.2?} ({:.0} req/s)", duration, AGENT_COUNT as f64 / duration.as_secs_f64());

    // 2. Message Ingestion Benchmark
    // We will have agents send messages to each other randomly.
    // To stress test, we spawn tasks.
    println!("🚀 Starting Ingestion Test...");
    let start = Instant::now();
    let mut handles = vec![];
    let barrier = Arc::new(Barrier::new(AGENT_COUNT));

    for i in 0..AGENT_COUNT {
        let client = client.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            let from = format!("BenchAgent_{}", i);
            let to = format!("BenchAgent_{}", (i + 1) % AGENT_COUNT);
            
            barrier.wait().await; // Synchronize start

            for j in 0..MESSAGES_PER_AGENT {
                let payload = json!({
                    "jsonrpc": "2.0", "id": j, "method": "tools/call",
                    "params": {
                        "name": "send_message",
                        "arguments": {
                            "from_agent": from, "to": [to],
                            "subject": format!("Bench Msg {}", j),
                            "body": "Performance testing payload data.",
                            "project_id": "bench"
                        }
                    }
                });
                let _ = client.post(SERVER_URL).json(&payload).send().await;
            }
        }));
    }
    for h in handles { h.await?; }
    let duration = start.elapsed();
    println!("✅ Ingestion:    {:.2?} ({:.0} msgs/s)", duration, TOTAL_REQUESTS as f64 / duration.as_secs_f64());

    // 3. Search Benchmark
    // Perform 1000 searches
    println!("🔍 Starting Search Test...");
    let start = Instant::now();
    let search_count = 1000;
    let mut handles = vec![];
    for i in 0..search_count {
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            let payload = json!({
                "jsonrpc": "2.0", "id": i, "method": "tools/call",
                "params": {
                    "name": "search_messages",
                    "arguments": { "query": "payload" }
                }
            });
            let _ = client.post(SERVER_URL).json(&payload).send().await;
        }));
    }
    for h in handles { h.await?; }
    let duration = start.elapsed();
    println!("✅ Search:       {:.2?} ({:.0} q/s)", duration, search_count as f64 / duration.as_secs_f64());

    // 4. Inbox Benchmark
    println!("📥 Starting Inbox Test...");
    let start = Instant::now();
    let mut handles = vec![];
    for i in 0..AGENT_COUNT {
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            let name = format!("BenchAgent_{}", i);
            let payload = json!({
                "jsonrpc": "2.0", "id": i, "method": "tools/call",
                "params": {
                    "name": "get_inbox",
                    "arguments": { "agent_name": name }
                }
            });
            let _ = client.post(SERVER_URL).json(&payload).send().await;
        }));
    }
    for h in handles { h.await?; }
    let duration = start.elapsed();
    println!("✅ Inbox:        {:.2?} ({:.0} req/s)", duration, AGENT_COUNT as f64 / duration.as_secs_f64());

    Ok(())
}
