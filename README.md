# 👁️ Allman
**A Hyper-Performance AI Agent "Nervous System"**

> *Inspired by the `mcp-agent-mail` pattern found in typical agent frameworks, but re-engineered from first principles for near-zero latency, extreme throughput, and data locality.*

## 🚀 Overview

Allman is a standalone **Model Context Protocol (MCP)** server designed to act as the high-speed communication backbone for large swarms of autonomous agents. 

While traditional agent mailboxes rely on slow file I/O or JSON parsing in Python/Node, **Allman** is built in **Rust** and acts as a neurological switchboard, routing thousands of thoughts and messages per second.

### Why Allman?
*   **Zero-Latency Communication**: Benchmarked at **12,000+ messages/sec**.
*   **Neural Search**: Built-in Tantivy indexing provides NRT (Near Real-Time) semantic search at **9,000+ queries/sec**.
*   **Integrated Intelligence**: Orchestrates local GPU resources (vLLM) to provide a "Brain-in-a-Box" via standard OpenAI protocols.
*   **Standard Compliance**: Fully compliant with MCP `tools/list` and `tools/call`.

## 🏗️ System Architecture

```ascii
                                    [ GPU 1: RTX 3080 Ti ]
                                             |
[ Agent Swarm ] <----(HTTP/JSON)----> [ vLLM Inference Engine ]
       |                               (Port 8001 / OpenAI API)
       |
(MCP / JSON-RPC)
       v
+-----------------------------------------------------------+
|                      ALLMAN SERVER (Rust)                 |
|                                                           |
|  [ MCP Controller ] -> Exposes tools/call, tools/list     |
|         |                                                 |
|  [ PostOffice (State) ]                                   |
|         |-- [ SQLite (WAL Mode) ] -> Persistence          |
|         |-- [ Tantivy Index ]     -> Search Engine        |
|         |-- [ Git Backup ]        -> Audit Trail          |
+-----------------------------------------------------------+
       ^
       | (Port 8000)
    [ Client / Dashboard ]
```

## 📊 Performance Benchmarks
*Tested on Dual NVIDIA RTX 3080 Ti Workstation.*

| Component | Operation | Throughput | Latency |
| :--- | :--- | :--- | :--- |
| **Server** | Message Ingestion | **11,937 msgs/s** | ~80µs |
| **Server** | Inbox Retrieval | **18,304 req/s** | ~50µs |
| **Server** | Full Text Search | **9,043 q/s** | ~110µs |
| **Brain** | Mistral 7B (AWQ) | **~60 tokens/s** | <500ms TTFT |

## 🛠️ Prerequisites

*   **Linux OS** (Ubuntu 22.04+ recommended)
*   **Docker** & `nvidia-container-toolkit` for GPU acceleration.
*   **NVIDIA GPU** (min 8GB VRAM for 7B Models).
*   **Rust Toolchain** (1.75+).

## 📥 Installation

1.  **Clone the Repository**
    ```bash
    git clone https://github.com/copyleftdev/allman.git
    cd allman
    ```

2.  **Configure GPU**
    Run the setup script to install NVIDIA Container Toolkit if not present:
    ```bash
    sudo ./setup_gpu.sh
    ```

3.  **Start Services (Allman + vLLM)**
    ```bash
    docker compose up -d
    ```
    *   This launches:
        *   **allman_server**: The Rust MCP server (Port 8000).
        *   **vllm_server**: The GPU Inference Engine (Port 8001).

    > **Note**: The default config uses `TheBloke/Mistral-7B-Instruct-v0.2-AWQ` heavily optimized for consumer GPUs. It requires ~6GB VRAM.

4.  **Verify Status**
    ```bash
    docker logs -f vllm_server
    # Wait for: "Application startup complete"
    ```

## 🧪 Usage

### Running Simulations
Allman comes with a **Cybersecurity Threat Simulation** to demonstrate swarm behavior.

```bash
cargo run --release --bin cyber_sim
```

This will:
1.  Register 30 Autonomous Agents with unique roles (Team Blue, Forensics, CISO, etc.).
2.  Connect them to the **vLLM** backend.
3.  Simulate a chaotic data exfiltration event.
4.  Agents will autonomously "think" (using vLLM) and "email" each other (using Allman) to resolve the incident.

### 2. "Black Friday" Retail Event 🎬
A cinematic chaos simulation with personality-driven agents.

```bash
cargo run --release --bin black_friday
```

This will:
1.  Register 5 distinct personas (Manager Dave, Shopper Karen, etc.).
2.  Simulate a retail meltdown event (stuck doors, POS crash).
3.  Demonstrate complex LLM role-playing and efficient vLLM usage (127.0.0.1).

### MCP Tools
To use Allman with your own agent framework (e.g., Claude Desktop, LangChain), connect to `http://localhost:8000/mcp`.

Available Tools:
*   `create_agent`: Register yourself.
*   `send_message(to, subject, body)`: Dispatch comms.
*   `get_inbox`: Check mail.
*   `search_messages(query)`: Find intel.

## 🤝 Contribution
Clean code is strictly enforced.
*   Ensure `cargo check` passes.
*   Run `cargo fmt` before committing.

## 📜 License
MIT
