Scaffold a new simulation binary. Simulation name: $ARGUMENTS

1. Read an existing simulation (e.g., `src/bin/cyber_sim.rs`) to understand the pattern.
2. Create `src/bin/$ARGUMENTS.rs` following this structure:
   - Constants: `SERVER_URL`, agent definitions, LLM config (if needed).
   - Helper: `mcp_call()` function for JSON-RPC requests.
   - Phase functions: setup agents, run simulation loop, collect results.
   - `#[tokio::main] async fn main()` orchestrating everything.
3. Register agents via `create_agent` with descriptive `name_hint` and `program` values.
4. Use `send_message` / `get_inbox` for inter-agent communication.
5. Print clear phase headers and metrics at the end.
6. Run `cargo check --all-targets` — zero warnings.
7. Test against running server.
8. Update `README.md` simulations section.
