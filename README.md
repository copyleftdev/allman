# Allman

A high-performance MCP agent mail server built in Rust. Named in honor of [Eric Allman](https://en.wikipedia.org/wiki/Eric_Allman), creator of Sendmail.

Allman acts as the communication backbone for large swarms of autonomous AI agents — lock-free message routing, near-real-time search, and a Git-backed audit trail, all exposed through the [Model Context Protocol](https://modelcontextprotocol.io/).

## Architecture

```
[ Agent Swarm ]                      [ vLLM / GPU Inference ]
       |                                   (Port 8001)
  MCP JSON-RPC                              |
       v                                    |
+----------------------------------------------+
|              ALLMAN SERVER (Rust)              |
|                                               |
|  MCP Controller   (tools/call, tools/list)    |
|       |                                       |
|  PostOffice                                   |
|    ├─ DashMap        lock-free hot state       |
|    ├─ Tantivy        NRT full-text search      |
|    └─ Git Actor      audit trail (OS thread)   |
|                                               |
|  Persist Worker      batched indexing pipeline  |
|    crossbeam-channel → Tantivy IndexWriter     |
+----------------------------------------------+
              Port 8000
```

**Hot path** (create_agent, send_message, get_inbox): DashMap read/write only — no disk I/O, no locks, microsecond latency.

**Persistence**: fire-and-forget via crossbeam channel. A dedicated OS thread batches Tantivy index writes and Git commits. The client never waits for disk.

## MCP Tools

Connect any MCP-compatible client to `http://localhost:8000/mcp`.

| Tool | Description |
|:---|:---|
| `create_agent` | Register an agent (project_key, name_hint, program, model) |
| `send_message` | Send a message (from_agent, to, subject, body, project_id) |
| `get_inbox` | Retrieve and drain unread messages for an agent |
| `search_messages` | Full-text search across all indexed messages |

## Getting Started

### Prerequisites

- **Rust** 1.75+
- **Docker** (optional — for containerized deployment and vLLM)
- **NVIDIA GPU** + `nvidia-container-toolkit` (optional — only for LLM inference)

### Build from Source

```bash
git clone https://github.com/copyleftdev/allman.git
cd allman
cargo build --release
```

### Run

```bash
# Start the server (binds to 0.0.0.0:8000)
cargo run --release
```

Configuration via environment variables (or `.env`):

| Variable | Default | Description |
|:---|:---|:---|
| `INDEX_PATH` | `allman_index` | Tantivy index directory |
| `REPO_ROOT` | `allman_repo` | Git audit trail directory |
| `RUST_LOG` | `allman=debug` | Log level filter |

### Docker

```bash
# Allman server only
docker compose up allman -d

# Allman + vLLM GPU inference
sudo ./setup_gpu.sh   # one-time NVIDIA toolkit install
docker compose up -d
```

## Simulations

Included binaries demonstrate swarm behavior with LLM-driven agents:

| Binary | Description |
|:---|:---|
| `cyber_sim` | 30-agent cybersecurity incident response (Blue Team, Forensics, CISO) |
| `black_friday` | Retail chaos simulation with personality-driven personas |
| `escrow_sim` | Real estate closing day with buyer, seller, and escrow officer |
| `swarm_stress` | High-concurrency stress test for throughput benchmarking |
| `benchmark` | Targeted latency and throughput measurements |

```bash
cargo run --release --bin cyber_sim
cargo run --release --bin swarm_stress
```

> **Note**: Simulations that use LLM inference require a running vLLM instance on port 8001. The stress test and benchmark run against Allman alone.

## Project Structure

```
src/
├── main.rs           Axum server, routing, MCP endpoint
├── controllers.rs    MCP tool dispatch (create_agent, send_message, etc.)
├── state.rs          PostOffice: DashMap state, Tantivy index, persist pipeline
├── models.rs         Data types (InboxEntry, Agent, Message)
├── git_actor.rs      Dedicated OS thread for Git audit commits
└── bin/
    ├── benchmark.rs
    ├── black_friday.rs
    ├── cyber_sim.rs
    ├── escrow_sim.rs
    ├── simulation.rs
    └── swarm_stress.rs
```

## Contributing

```bash
cargo fmt
cargo check   # must pass clean
cargo test
```

## License

MIT
