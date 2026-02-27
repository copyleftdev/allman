use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::sleep;

const SERVER_URL: &str = "http://localhost:8000/mcp";
const LLM_API_URL: &str = "http://127.0.0.1:8001/v1/chat/completions";
const MODEL: &str = "TheBloke/Mistral-7B-Instruct-v0.2-AWQ";
const API_KEY: &str = "sk-test-123";

struct Agent {
    name: String,
    role: String,
    persona: String,
    project_id: String,
    client: Client,
    history: Vec<Value>,
}

impl Agent {
    async fn new(name: &str, role: &str, persona: &str) -> Self {
        let client = Client::new();
        // Register agent
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "create_agent",
                "arguments": {
                    "project_key": "black_friday_2026",
                    "name_hint": name,
                    "task_description": role,
                    "model": MODEL
                }
            }
        });

        let mut project_id = "black_friday_2026".to_string();

        // Registration call...
        if let Ok(resp) = client.post(SERVER_URL).json(&payload).send().await {
            if let Ok(json) = resp.json::<Value>().await {
                if let Some(pid) = json
                    .get("result")
                    .and_then(|r| r.get("content"))
                    .and_then(|c| c.get(0))
                    .and_then(|t| t.get("text"))
                    .and_then(|s| s.as_str())
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .and_then(|v| v.get("project_id").cloned())
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                {
                    project_id = pid;
                }
            }
        }
        println!("[{}] enters the store.", name);

        Agent {
            name: name.to_string(),
            role: role.to_string(),
            persona: persona.to_string(),
            project_id,
            client,
            history: Vec::new(),
        }
    }

    async fn check_inbox(&mut self) -> anyhow::Result<usize> {
        let payload = json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "get_inbox", "arguments": { "agent_name": self.name } }
        });

        let resp = self
            .client
            .post(SERVER_URL)
            .json(&payload)
            .send()
            .await?
            .json::<Value>()
            .await?;

        let mut count = 0;
        if let Some(content_str) = resp
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.get(0))
            .and_then(|t| t.get("text"))
            .and_then(|s| s.as_str())
        {
            let parsed: Value = serde_json::from_str(content_str)?;
            let messages = parsed["messages"].as_array().cloned().unwrap_or_default();
            count = messages.len();

            for msg in messages {
                let from = msg
                    .get("from_agent")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let body = msg.get("body").and_then(|v| v.as_str()).unwrap_or("");
                let subject = msg.get("subject").and_then(|v| v.as_str()).unwrap_or("");

                println!(
                    "\n📨 {} received mail from {}: {}",
                    self.name, from, subject
                );

                // Add to history
                self.history
                    .push(json!({"role": "user", "content": format!("From {}: {}", from, body)}));

                // Reply Logic
                self.generate_and_send(from, "").await?;
            }
        }
        Ok(count)
    }

    async fn generate_and_send(&mut self, to: &str, extra_context: &str) -> anyhow::Result<()> {
        // Construct Cinematic System Prompt
        let system_message = json!({
            "role": "user",
            "content": format!(
                "You are {}. Role: {}. \
                SCENARIO: It is Black Friday. The store is packed. Chaos. \
                PERSONA: {}. \
                Start with a 'thought' in *italics* then write your message. \
                Keep it dramatic but short.",
                self.name, self.role, self.persona
            )
        });

        // Context Injection
        if !extra_context.is_empty() {
            // We inject context as a "User" observation to comply with vLLM
            self.history
                .push(json!({"role": "user", "content": format!("EVENT: {}", extra_context)}));
        }

        // Window Helper
        let start = if self.history.len() > 6 {
            self.history.len() - 6
        } else {
            0
        };
        let history_slice = &self.history[start..];

        let mut messages = vec![system_message];
        messages.extend(history_slice.iter().cloned());

        let llm_payload = json!({
            "model": MODEL,
            "messages": messages,
            "temperature": 0.8, // Chaotic
            "max_tokens": 150
        });

        print!("[{}] 🤔 Action...", self.name);
        use std::io::Write;
        std::io::stdout().flush().unwrap();

        let response_text = match self
            .client
            .post(LLM_API_URL)
            .header("Authorization", format!("Bearer {}", API_KEY))
            .json(&llm_payload)
            .send()
            .await
        {
            Ok(resp) => {
                let body: Value = resp.json().await.unwrap_or(json!({}));
                body.get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "Error parsing vLLM".to_string())
            }
            Err(e) => {
                println!("[{}] DEBUG: Error call LLM: {}", self.name, e);
                format!("(Debug) Error calling vLLM: {}", e)
            }
        };

        println!(" Done.");

        self.history
            .push(json!({"role": "assistant", "content": response_text}));

        let subject = "Black Friday Update".to_string();

        let mcp_payload = json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
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

        let _ = self
            .client
            .post(SERVER_URL)
            .json(&mcp_payload)
            .send()
            .await?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("--- 🎬 SCENE: BLACK FRIDAY APOCALYPSE ---");
    println!("--- 🏬 LOCATION: SuperMart 3000 ---");

    // Casting
    let mut manager = Agent::new(
        "Manager_Dave",
        "Store Manager",
        "Stressed, sweating, trying to prevent a riot. Hates his life.",
    )
    .await;
    let mut karen = Agent::new(
        "Shopper_Karen",
        "Entitled Customer",
        "Demanding, oblivious to tech issues, wants to speak to the manager IMMEDIATELY.",
    )
    .await;
    let mut kevin = Agent::new(
        "Shopper_Kevin",
        "Confused Customer",
        "Lost, looking for the bathroom, holding a frozen turkey.",
    )
    .await;
    let mut hacker = Agent::new(
        "Hacker_Zero",
        "Cybercriminal",
        "Cool, calculated, typing fast. Wearing a hoodie in the food court.",
    )
    .await;
    let mut cashier = Agent::new(
        "Cashier_Pat",
        "Teenage Employee",
        "Bored, secretly helping the hacker, hates the manager.",
    )
    .await;

    // --- ACT I: The Rush ---
    println!("\n--- 🕐 ACT I: THE DOORS OPEN ---");
    karen
        .generate_and_send(
            "Manager_Dave",
            "The automatic doors are stuck! I've been waiting 3 hours!",
        )
        .await?;
    manager.check_inbox().await?; // Dave reacts

    sleep(Duration::from_millis(1000)).await;

    // --- ACT II: The Glitch ---
    println!("\n--- 🕐 ACT II: THE POS CRASH ---");
    hacker
        .generate_and_send(
            "Cashier_Pat",
            "Injecting payload into Register 4. Distract Dave.",
        )
        .await?;
    cashier.check_inbox().await?; // Pat reacts
    cashier
        .generate_and_send(
            "Manager_Dave",
            "Uh, boss? The registers are all showing skulls.",
        )
        .await?;

    sleep(Duration::from_millis(1000)).await;

    manager.check_inbox().await?; // Dave gets hit with bad news
    kevin
        .generate_and_send(
            "Manager_Dave",
            "Excuse me, where are the batteries? Also the registers are smoking.",
        )
        .await?;

    sleep(Duration::from_millis(1000)).await;

    // --- ACT III: The Meltdown ---
    println!("\n--- 🕐 ACT III: TOTAL CHAOS ---");
    karen.check_inbox().await?; // Maybe Dave replied?
    karen
        .generate_and_send(
            "Manager_Dave",
            "THIS IS UNACCEPTABLE. I AM CALLING CORPORATE.",
        )
        .await?;

    hacker
        .generate_and_send(
            "Manager_Dave",
            "We have your transaction logs. Wire 50 BTC or we leak the credit cards.",
        )
        .await?;

    manager.check_inbox().await?; // Dave gets the ransom

    // Final thoughts
    println!("\n--- 🎬 SCENE END ---");
    Ok(())
}
