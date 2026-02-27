use rand::seq::SliceRandom;
use rand::Rng;
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
    team: String,
}

impl Agent {
    async fn new(name: &str, role: &str, team: &str) -> Self {
        let client = Client::new();
        // Register agent
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "create_agent",
                "arguments": {
                    "project_key": "cyber_sim_v1",
                    "name_hint": name,
                    "task_description": role,
                    "model": MODEL
                }
            }
        });

        let mut project_id = "cyber_sim_v1".to_string();

        let resp = client.post(SERVER_URL).json(&payload).send().await;
        if let Ok(r) = resp {
            println!("[{}] Registered ({})", name, team);
            if let Ok(json) = r.json::<Value>().await {
                if let Some(pid) = json
                    .get("result")
                    .and_then(|r| r.get("content"))
                    .and_then(|c| c.get(0))
                    .and_then(|text| text.get("text"))
                    .and_then(|s| s.as_str())
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .and_then(|v| v.get("project_id").cloned())
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                {
                    project_id = pid;
                }
            }
        }

        Agent {
            name: name.to_string(),
            role: role.to_string(),
            project_id,
            client,
            history: Vec::new(),
            team: team.to_string(),
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
                    .get("from")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let body = msg.get("body").and_then(|v| v.as_str()).unwrap_or("");
                let subject = msg.get("subject").and_then(|v| v.as_str()).unwrap_or("");

                // Print only first line of subject to reduce noise
                println!("\n[{}] 📩 From {}: {}", self.name, from, subject);

                // Add to history
                self.history
                    .push(json!({"role": "user", "content": format!("From {}: {}", from, body)}));

                // PROBABILISTIC REPLY: In a 30-person thread, you don't always reply.
                // 30% chance to reply unless addressed directly?
                let should_reply = if subject.to_lowercase().contains(&self.name.to_lowercase()) {
                    true
                } else {
                    rand::thread_rng().gen_bool(0.3)
                };

                if should_reply {
                    self.generate_and_send(from, "").await?;
                }
            }
        }
        Ok(count)
    }

    async fn generate_and_send(&mut self, to: &str, extra_context: &str) -> anyhow::Result<()> {
        let system_message = json!({
            "role": "system",
            "content": format!("You are {}. Role: {}. Team: {}. {}. \
            You are in a high-stress cybersecurity incident. \
            Detecting compromised vendor 'AcmeCorp'. \
            Keep responses very short technical updates or questions. Use urgency.",
            self.name, self.role, self.team, extra_context)
        });

        let start = if self.history.len() > 4 {
            self.history.len() - 4
        } else {
            0
        };
        let history_slice = &self.history[start..];

        let mut messages = vec![system_message];
        messages.extend(history_slice.iter().cloned());

        let llm_payload = json!({
            "model": MODEL,
            "messages": messages,
            "temperature": 0.7,
            "max_tokens": 256
        });

        print!("[{}] 🤔 (vLLM)...", self.name);
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
                let body_text = resp.text().await.unwrap_or_default();
                let body: Value = serde_json::from_str(&body_text).unwrap_or(json!({}));
                // OpenAI Parser
                body.get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "Error parsing vLLM".to_string())
            }
            Err(_) => "(Simulated) Network Error".to_string(),
        };

        println!(" Sent.");

        self.history
            .push(json!({"role": "assistant", "content": response_text}));

        let subject = if response_text.len() > 20 {
            let preview: String = response_text.chars().take(10).collect();
            format!("Re: Inc-## {}", preview)
        } else {
            "Re: Incident Update".to_string()
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

        let _ = self
            .client
            .post(SERVER_URL)
            .json(&mcp_payload)
            .send()
            .await?;
        Ok(())
    }

    async fn initiate(&mut self, to: &str, topic: &str) -> anyhow::Result<()> {
        self.history
            .push(json!({"role": "user", "content": format!("ALERT: {}", topic)}));
        self.generate_and_send(to, "CRITCAL ALERT DETECTED").await
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("--- 🛡️  Cybersecurity Incident Simulator (30 Agents) ---");

    let mut agents = Vec::new();

    // 1. Leadership (3)
    agents.push(
        Agent::new(
            "CISO_Sarah",
            "Chief Information Security Officer. Demanding updates. Thinking about legal.",
            "Leadership",
        )
        .await,
    );
    agents.push(
        Agent::new(
            "VP_Eng_Tom",
            "VP Engineering. worried about uptime. skeptical of breach.",
            "Leadership",
        )
        .await,
    );
    agents.push(
        Agent::new(
            "Legal_Mike",
            "General Counsel. worried about liability. don't admit fault.",
            "Leadership",
        )
        .await,
    );

    // 2. Incident Response Team (10)
    for i in 1..=10 {
        agents.push(
            Agent::new(
                &format!("IR_Lead_{}", i),
                "Senior Incident Responder. Technical, stressed, analyzing logs.",
                "BlueTeam",
            )
            .await,
        );
    }

    // 3. Forensics (5)
    for i in 1..=5 {
        agents.push(
            Agent::new(
                &format!("Forensics_{}", i),
                "Forensic Analyst. Deep diving into images. Looking for rootkits.",
                "Forensics",
            )
            .await,
        );
    }

    // 4. Platform Eng (5)
    for i in 1..=5 {
        agents.push(
            Agent::new(
                &format!("Platform_{}", i),
                "DevOps Engineer. Restarting servers. Firewall rules.",
                "Ops",
            )
            .await,
        );
    }

    // 5. External/Contractors (5)
    for i in 1..=5 {
        agents.push(
            Agent::new(
                &format!("AcmeCorp_Rep_{}", i),
                "Vendor Representative. Denying they are compromised. Unhelpful.",
                "Vendor",
            )
            .await,
        );
    }

    // 6. Threat Intel (2)
    agents.push(
        Agent::new(
            "Intel_Alice",
            "Threat Intelligence. Correlating IOCs.",
            "Intel",
        )
        .await,
    );
    agents.push(
        Agent::new(
            "Intel_Bob",
            "Threat Intelligence. Monitoring Dark Web.",
            "Intel",
        )
        .await,
    );

    println!("\n✅ All 30 Agents Registered.\n");

    // Start Event
    let _ = agents[5]
        .initiate("CISO_Sarah", "DETECTED ZERO DAY IN VENDOR ID 99")
        .await; // IR Lead triggering

    // Simulation Loop
    for minute in 1..20 {
        println!("\n--- 🕐 T+{} Minutes ---", minute * 10);

        let mut rng = rand::thread_rng();
        agents.shuffle(&mut rng);

        // Batch processing to be kind to the single-threaded server/Ollama
        for agent in agents.iter_mut().take(5) {
            // Only 5 active per tick to reduce spam
            let _ = agent.check_inbox().await;
            sleep(Duration::from_millis(500)).await;
        }

        // Random Injections
        if minute == 3 {
            println!(
                "🚨 INJECTION: Firewall logs show 50GB exfil to IP 1.2.3.4 (AcmeCorp VPN range)"
            );
            let _ = agents[3]
                .initiate("CISO_Sarah", "Data Exfil Confirmed")
                .await;
        }
    }

    println!("\n--- 🏁 Incident Contained (Simulation End) ---");
    Ok(())
}
