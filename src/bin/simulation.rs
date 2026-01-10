use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::sleep;

const SERVER_URL: &str = "http://localhost:8000/mcp";
const LLM_API_URL: &str = "http://localhost:8001/v1/chat/completions";
const MODEL: &str = "TheBloke/Mistral-7B-Instruct-v0.2-AWQ"; 
const API_KEY: &str = "sk-test-123";

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
                    "project_key": "simulation",
                    "name_hint": name,
                    "task_description": role,
                    "model": MODEL
                }
            }
        });
        // ... (Registration Logic Unchanged) ...
        let mut project_id = "simulation".to_string(); 
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
                        println!("[{}] Acquired Project ID: {}", name, project_id);
                 }
             }
        } else {
            println!("[{}] Failed to register (is server up?)", name);
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
        // ... (check_inbox logic unchanged) ...
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "get_inbox", "arguments": { "agent_name": self.name } }
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
                println!("\n[{}] Received mail from {}: {}", self.name, from, body);
                
                self.history.push(json!({"role": "user", "content": format!("Message from {}: {}", from, body)}));
                self.generate_and_send(from).await?;
            }
        }
        Ok(count)
    }

    async fn generate_and_send(&mut self, to: &str) -> anyhow::Result<()> {
        let system_message = json!({
            "role": "system", 
            "content": format!("You are {}. Role: {}. Respond to email concise.", self.name, self.role)
        });
        
        // Build messages array
        let mut messages = vec![system_message];
        messages.extend(self.history.iter().cloned());
        // Last user message was added in check_inbox

        let llm_payload = json!({
            "model": MODEL,
            "messages": messages,
            "temperature": 0.7,
            "max_tokens": 512
        });

        println!("[{}] Thinking (via vLLM)...", self.name);
        
        let response_text = match self.client.post(LLM_API_URL)
            .header("Authorization", format!("Bearer {}", API_KEY))
            .json(&llm_payload)
            .send()
            .await 
        {
            Ok(resp) => {
                let status = resp.status();
                let body_text = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                     println!("[{}] vLLM Error {}: {}", self.name, status, body_text);
                     "I cannot think.".to_string()
                } else {
                    let body: Value = serde_json::from_str(&body_text).unwrap_or(json!({}));
                    // OpenAI format: choices[0].message.content
                    body.get("choices")
                        .and_then(|c| c.get(0))
                        .and_then(|c| c.get("message"))
                        .and_then(|m| m.get("content"))
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| {
                             println!("[{}] Unexpected JSON: {}", self.name, body_text);
                             "Error parsing response".to_string()
                        })
                }
            },
            Err(e) => {
                 println!("[{}] vLLM Connection Error: {}", self.name, e);
                 "I cannot think (service offline).".to_string()
            }
        };

        println!("[{}] Generated: {}", self.name, response_text);
        self.history.push(json!({"role": "assistant", "content": response_text}));

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
                    "subject": "Re: Conversation",
                    "body": response_text,
                    "project_id": self.project_id
                }
            }
        });

        let resp = self.client.post(SERVER_URL).json(&mcp_payload).send().await?;
        println!("[{}] Sent msg response: {:?}", self.name, resp.text().await);

        Ok(())
    }
    
    async fn initiate(&mut self, to: &str, topic: &str) -> anyhow::Result<()> {
        self.history.push(json!({"role": "system", "content": format!("You are starting a conversation about: {}", topic)}));
        self.generate_and_send(to).await
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("Starting Agent Simulation...");
    println!("Ensure 'allman' server is running on port 8000");
    println!("Ensure 'ollama' is running on port 11434");

    let mut agent_a = Agent::new("Planner", "You organize events. You are planning a surprise party.").await;
    let mut agent_b = Agent::new("Baker", "You make cakes. You need details about the party.").await;

    // Kickoff
    println!("\n--- Initiation ---");
    agent_a.initiate("Baker", "I need to order a cake for a surprise party next Friday.").await?;

    // Loop
    for i in 0..6 {
        println!("\n--- Turn {} ---", i);
        sleep(Duration::from_secs(2)).await;
        
        // B checks inbox
        if agent_b.check_inbox().await? > 0 {
             sleep(Duration::from_secs(1)).await;
        }

        // A checks inbox
        if agent_a.check_inbox().await? > 0 {
             sleep(Duration::from_secs(1)).await;
        }
    }

    println!("\nSimulation Complete.");
    Ok(())
}
