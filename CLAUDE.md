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

## Git Flow (Non-Negotiable)

All new work goes through branches and PRs. Never push directly to master.

```
master (protected)
  └── feature/<name>    — new capabilities
  └── fix/<name>        — bug fixes
  └── refactor/<name>   — structural improvements
  └── optimize/<name>   — performance work
  └── test/<name>       — test-only additions
```

1. `git checkout master && git pull origin master`
2. `git checkout -b <type>/<name>`
3. Work in small, atomic commits. Each commit must pass `cargo check --all-targets`.
4. Run `/preflight` before opening a PR.
5. Run `/pr` to create the pull request. Never merge your own PR without review.
6. If master moves ahead: `git fetch origin && git rebase origin/master`

## Testing Philosophy

**Nothing is assumed. Everything is asserted by hypothesis.**

- Write tests BEFORE implementation (TDD). The test defines the contract.
- Every hypothesis about behavior must be captured as a test. Use `/test-hypothesis` for this.
- Tests must assert specific outcomes, not just "doesn't panic."
- Characterization tests must exist before any refactoring begins. Use `/refactor` for this.
- Performance claims must be backed by benchmark measurements. Use `/optimize` for this.
- Never delete or weaken a passing test without explicit direction.

Test locations:
- Unit tests: `#[cfg(test)] mod tests { ... }` inside each `src/*.rs` file.
- Integration tests: `tests/` directory (for HTTP-level MCP endpoint tests).
- Benchmarks: `src/bin/benchmark.rs` and `src/bin/swarm_stress.rs` (run against live server).

```bash
cargo test                          # run all unit + integration tests
cargo test -- --nocapture           # with stdout
cargo test test_name                # run a specific test
```

## Workflow: Before Modifying Core Files

Before changing `controllers.rs`, `state.rs`, `models.rs`, or `git_actor.rs`:

1. Create a feature branch: `/feature-branch <name>`
2. Read the file and understand the current contract.
3. Write characterization tests for the current behavior (if they don't exist).
4. Plan the change — identify which hot path, search path, or audit path is affected.
5. Implement minimally.
6. Run `cargo check --all-targets` — zero warnings.
7. Run `cargo test` — all tests pass.
8. Run `cargo run --release --bin benchmark` to verify no performance regression.
9. Commit with a descriptive message.
10. Open a PR: `/pr`

## Slash Commands

| Command | Purpose |
| ------- | ------- |
| `/bench` | Run benchmark suite, report throughput, flag regressions |
| `/preflight` | Pre-commit checks: fmt, clippy, check, test, diff summary |
| `/deep-review <scope>` | Adversarial code review with hypothesis-driven findings |
| `/optimize <target>` | Performance optimization with before/after proof |
| `/refactor <target>` | Test-first refactoring with characterization tests |
| `/test-hypothesis <claim>` | TDD assertion of a specific behavior |
| `/feature-branch <name>` | Start new work with proper Git flow |
| `/pr` | Open a pull request with structured description |
| `/add-tool <name>` | Guided MCP tool scaffolding |
| `/add-simulation <name>` | Scaffold a new simulation binary |
| `/review-hot-path` | Audit hot path for performance regressions |

## Runtime Artifacts (gitignored)

- `allman_index/` — Tantivy search index (regenerated on startup)
- `allman_repo/` — Git audit trail (created on startup)
- `target/` — Rust build cache

## Environment Variables

- `RUST_LOG` — tracing filter (e.g., `allman=info`, `allman=debug`)
- `INDEX_PATH` — Tantivy index directory (default: `allman_index`)
- `REPO_ROOT` — Git audit repo directory (default: `allman_repo`)
