Run the Allman benchmark suite against a running server.

1. Check that the server is running: `curl -sf http://localhost:8000/health`
2. If not running, start it: `RUST_LOG=allman=info cargo run --release --bin allman`
3. Run the lightweight benchmark: `cargo run --release --bin benchmark`
4. Run the 2000-agent swarm stress test: `cargo run --release --bin swarm_stress`
5. Report throughput numbers for: registration, messaging, inbox, and search.
6. Flag any errors or regressions compared to baseline (289K msg/s, 247K inbox/s, 89K q/s, 25K reg/s).
