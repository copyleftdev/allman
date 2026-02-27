Add a new MCP tool to the Allman server. Tool name: $ARGUMENTS

1. Read `src/controllers.rs` to understand the existing dispatch pattern.
2. Read `src/state.rs` to understand available state (DashMap, Tantivy, crossbeam channel).
3. Read `src/models.rs` to check if new data types are needed.
4. Plan the new tool:
   - What arguments does it take? Which are required vs optional?
   - Does it touch the hot path (DashMap), search path (Tantivy), or audit path (Git)?
   - What does it return?
   - What are the failure modes?
5. Implement the tool function in `controllers.rs`.
6. Add the tool to the `match tool_name` dispatch and `tools/list` response.
7. Add any new types to `models.rs` if needed.
8. Run `cargo check --all-targets` — zero warnings.
9. Test with curl: `curl -s http://localhost:8000/mcp -H 'Content-Type: application/json' -d '...'`
10. Update `CLAUDE.md` tool table and `README.md`.
