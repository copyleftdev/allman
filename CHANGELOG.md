# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2024-12-01

### Added

- MCP JSON-RPC 2.0 server on Axum (port 8000)
- Four MCP tools: `create_agent`, `send_message`, `get_inbox`, `search_messages`
- Lock-free hot path via DashMap (agents, inboxes, projects)
- Tantivy NRT full-text search with batched persistence worker
- Git audit trail via dedicated OS thread (git2)
- Crossbeam channel pipeline for async persistence
- Docker and docker-compose deployment (Allman + vLLM)
- Simulation binaries: `cyber_sim`, `black_friday`, `escrow_sim`, `swarm_stress`, `benchmark`
- Runnable curl examples in `examples/`

### Removed

- SQLite/sqlx dependency (replaced by DashMap)
- SQL migrations directory
