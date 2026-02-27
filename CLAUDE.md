# Allman — MCP Agent Mail Server

High-performance MCP (Model Context Protocol) agent mail server. Lock-free message routing, NRT full-text search, Git audit trail. Written in Rust.

## Architecture

```
HTTP POST /mcp
     │
     ▼
┌─ Axum Router ─────────────────────────┐
│  /health  GET  → "OK"                 │
│  /mcp     POST → handle_mcp_request() │
└────────────┬──────────────────────────┘
             │ JSON-RPC 2.0
             ▼
┌─ controllers.rs ──────────────────────┐
│  tools/call → dispatch by tool name   │
│  tools/list → enumerate tools         │
└──┬────────┬────────┬─────────────────┘
   │        │        │
   ▼        ▼        ▼
DashMap   Tantivy   Git Actor
(hot)     (NRT)     (audit)
```

**Hot path** (DashMap): `create_agent`, `send_message`, `get_inbox` — zero disk I/O, lock-free.
**Search path** (Tantivy): `search_messages` — NRT reader, batched index writes via crossbeam channel.
**Audit path** (Git): agent profiles committed async via dedicated OS thread.

## Key Directories

```
src/
├── main.rs          — Axum server, router, entry point
├── controllers.rs   — MCP JSON-RPC handler, 4 tool implementations
├── state.rs         — PostOffice (DashMap + Tantivy + crossbeam pipeline)
├── models.rs        — InboxEntry, Agent, Message, FileReservation
├── git_actor.rs     — Git commit actor on dedicated OS thread
└── bin/
    ├── benchmark.rs     — throughput benchmark (100 agents, 1K msgs)
    ├── swarm_stress.rs  — 2000-agent stress test (4 phases)
    ├── cyber_sim.rs     — LLM-driven incident response simulation
    ├── black_friday.rs  — LLM retail chaos simulation
    ├── escrow_sim.rs    — LLM escrow negotiation simulation
    └── simulation.rs    — general simulation harness
examples/                — runnable curl scripts (01–05)
```

## MCP Tools (the entire API surface)

| Tool | Required Args | Optional Args | Returns |
| ---- | ------------- | ------------- | ------- |
| `create_agent` | `project_key` | `name_hint`, `program`, `model` | `{ id, project_id, name, program, model }` |
| `send_message` | `from_agent`, `to` (array) | `subject`, `body`, `project_id` | `{ id, status }` |
| `get_inbox` | `agent_name` | — | `[{ id, from, subject, body, timestamp }]` |
| `search_messages` | `query` | `limit` | array of Tantivy docs (fields as arrays) |

**Critical invariants:**
- `to` is always a JSON array, even for one recipient
- `get_inbox` is a destructive drain — messages removed after read
- `project_id` is deterministic: `Uuid::new_v5(NAMESPACE_DNS, project_key)`
- Search is eventually consistent (~200ms after send)

## Common Commands

```bash
# Build
cargo build --release
cargo check --all-targets

# Run server
RUST_LOG=allman=info cargo run --release --bin allman

# Run benchmarks (server must be running on :8000)
cargo run --release --bin benchmark
cargo run --release --bin swarm_stress

# Run simulations (server must be running, needs LLM for cyber/black_friday/escrow)
cargo run --release --bin cyber_sim
cargo run --release --bin black_friday
cargo run --release --bin escrow_sim

# Format and lint
cargo fmt
cargo clippy --all-targets

# Quick health check
curl http://localhost:8000/health

# Quick MCP test
curl -s http://localhost:8000/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

## Standards

- `cargo check --all-targets` must pass with **zero warnings** before commit.
- `cargo fmt` on all files.
- No new dependencies without justification.
- Hot-path code (controllers.rs, state.rs) must avoid disk I/O and heap allocation where possible.
- Commit messages use imperative mood: `fix:`, `feat:`, `docs:`, `refactor:`.

## Workflow: Before Modifying Core Files

Before changing `controllers.rs`, `state.rs`, `models.rs`, or `git_actor.rs`:
1. Read the file and understand the current contract.
2. Plan the change — identify which hot path, search path, or audit path is affected.
3. Implement minimally.
4. Run `cargo check --all-targets` — zero warnings.
5. Run `cargo run --release --bin benchmark` to verify no performance regression.
6. Commit with a descriptive message.

## Runtime Artifacts (gitignored)

- `allman_index/` — Tantivy search index (regenerated on startup)
- `allman_repo/` — Git audit trail (created on startup)
- `target/` — Rust build cache

## Environment Variables

- `RUST_LOG` — tracing filter (e.g., `allman=info`, `allman=debug`)
- `INDEX_PATH` — Tantivy index directory (default: `allman_index`)
- `REPO_ROOT` — Git audit repo directory (default: `allman_repo`)
