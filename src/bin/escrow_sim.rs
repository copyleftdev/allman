use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::sleep;
use rand::seq::SliceRandom;

const SERVER_URL: &str = "http://localhost:8000/mcp";
const OLLAMA_URL: &str = "http://localhost:11434/api/generate";
const MODEL: &str = "mistral:latest"; 

struct Agent {
    name: String,
    role: String,
    project_id: String,
    client: Client,
    history: Vec<Value>, 
}

impl Agent {
    async fn new(name: &str, role: &str) -> Self {
        let client = Client::new();
        // Register agent
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "create_agent",
                "arguments": {
                    "project_key": "escrow_chaos",
                    "name_hint": name,
                    "task_description": role,
                    "model": MODEL
                }
            }
        });

        let mut project_id = "escrow_chaos".to_string(); 

        let resp = client.post(SERVER_URL).json(&payload).send().await;
        if let Ok(r) = resp {
             println!("[{}] Registered: {:?}", name, r.status());
             if let Ok(json) = r.json::<Value>().await {
                 if let Some(pid) = json.get("result")
                    .and_then(|r| r.get("content"))
                    .and_then(|c| c.get(0))
                    .and_then(|text| text.get("text"))
                    .and_then(|s| s.as_str())
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .and_then(|v| v.get("project_id").cloned())
                    .and_then(|v| v.as_str().map(|s| s.to_string())) {
                        project_id = pid;
                        // println!("[{}] Project ID: {}", name, project_id);
                 }
             }
        }

        Agent {
            name: name.to_string(),
            role: role.to_string(),
            project_id,
            client,
            history: Vec::new(),
        }
    }

    async fn check_inbox(&mut self) -> anyhow::Result<usize> {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "get_inbox",
                "arguments": {
                    "agent_name": self.name
                }
            }
        });

        let resp = self.client.post(SERVER_URL).json(&payload).send().await?.json::<Value>().await?;
        
        let mut count = 0;
        if let Some(content_str) = resp.get("result").and_then(|r| r.get("content"))
            .and_then(|c| c.get(0)).and_then(|t| t.get("text")).and_then(|s| s.as_str()) {
            
            let messages: Vec<Value> = serde_json::from_str(content_str)?;
            count = messages.len();

            for msg in messages {
                let from = msg.get("from").and_then(|v| v.as_str()).unwrap_or("unknown");
                let body = msg.get("body").and_then(|v| v.as_str()).unwrap_or("");
                let subject = msg.get("subject").and_then(|v| v.as_str()).unwrap_or("");
                
                println!("\n📬 [{}] Received from {}: \"{}\"", self.name, from, subject);
                
                // Add to history
                self.history.push(json!({"role": "user", "content": format!("From {}: {}", from, body)}));

                // Heuristic: If it's the escrow officer, reply promptly. If it's a nagging client, maybe delay?
                // For Chaos Sim: Always reply!
                
                // Add some chaotic context?
                let context = if self.name == "Eddie" {
                     "You are extremely busy. Another file just fell on the floor. The phone is ringing."
                } else {
                     ""
                };
                
                self.generate_and_send(from, context).await?;
            }
        }
        Ok(count)
    }

    async fn generate_and_send(&mut self, to: &str, extra_context: &str) -> anyhow::Result<()> {
        let system_prompt = format!("You are {}. Role: {}. {}. Keep emails short, urgent, and slightly chaotic.", self.name, self.role, extra_context);
        
        // Truncate history to keep context manageable
        let start = if self.history.len() > 6 { self.history.len() - 6 } else { 0 };
        let history_slice = &self.history[start..];

        let prompt = format!("{}\n\nLast Messages:\n{:?}\n\nWrite a response to {}.", system_prompt, history_slice, to);

        let ollama_payload = json!({
            "model": MODEL,
            "prompt": prompt,
            "stream": false
        });

        print!("[{}] Typing...", self.name);
        use std::io::Write;
        std::io::stdout().flush().unwrap();

        let response_text = match self.client.post(OLLAMA_URL).json(&ollama_payload).send().await {
            Ok(resp) => {
                let body_text = resp.text().await.unwrap_or_default();
                let body: Value = serde_json::from_str(&body_text).unwrap_or(json!({}));
                body.get("response").and_then(|v| v.as_str()).unwrap_or("...").to_string()
            },
            Err(_) => "(Simulated) Network Error".to_string()
        };

        println!(" Sent!");
        // println!("> {}", response_text.replace("\n", " ").chars().take(60).collect::<String>());

        self.history.push(json!({"role": "assistant", "content": response_text}));

        let subject = if response_text.len() > 20 {
            format!("Re: ...{}", &response_text[..15])
        } else {
            "Update".to_string()
        };

        // Send message
        let mcp_payload = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "send_message",
                "arguments": {
                    "from_agent": self.name,
                    "to": [to],
                    "subject": subject,
                    "body": response_text,
                    "project_id": self.project_id
                }
            }
        });

        let _ = self.client.post(SERVER_URL).json(&mcp_payload).send().await?;
        Ok(())
    }
    
    async fn initiate(&mut self, to: &str, topic: &str) -> anyhow::Result<()> {
        self.history.push(json!({"role": "system", "content": format!("Event: {}", topic)}));
        self.generate_and_send(to, "URGENT START").await
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("--- 🏠 Thriving Chaotic Escrow Simulator ---");
    
    let mut alice = Agent::new("Alice", "Impatient Buyer. You wired the money an hour ago and want the keys NOW. You are texting your mover.").await;
    let mut bob = Agent::new("Bob", "Anxious Seller. You need the proceeds to close on your new house in Florida today. You are skeptical of banks.").await;
    let mut eddie = Agent::new("Eddie", "Overwhelmed Escrow Officer. You have 15 closings today. The printer is jammed. You are trying to be professional but you are sweating.").await;

    // Chaos Starter
    alice.initiate("Eddie", "Where are my keys?").await?;
    sleep(Duration::from_millis(1500)).await;
    bob.initiate("Eddie", "Did the wire hit?").await?;

    let _agents = vec!["Alice", "Bob", "Eddie"];
    
    // Simulation Loop
    for turn in 1..8 {
        println!("\n--- ⏳ Hour {} ---", turn + 8); // 9AM to 5PM logic in turns
        sleep(Duration::from_secs(2)).await;

        // Randomize who checks mail to simulate async chaos
        let mut rng = rand::thread_rng();
        let mut indices: Vec<usize> = (0..3).collect();
        indices.shuffle(&mut rng);

        for i in indices {
            match i {
                0 => { let _ = alice.check_inbox().await; },
                1 => { let _ = bob.check_inbox().await; },
                2 => { let _ = eddie.check_inbox().await; },
                _ => {}
            }
            sleep(Duration::from_millis(500)).await;
        }

        // Random Chaos Event
        if turn == 3 {
             println!("\n🚨 EVENT: The Federal Reserve wire system is glitching!");
             eddie.history.push(json!({"role": "system", "content": "ALERT: FedWire delays reported."}));
             eddie.generate_and_send("Alice", "Massive system failure context").await?;
             eddie.generate_and_send("Bob", "Massive system failure context").await?;
        }
    }

    println!("\n--- 🏁 End of Business Day ---");
    Ok(())
}
