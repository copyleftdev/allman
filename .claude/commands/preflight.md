Run the full preflight check before committing changes.

1. `cargo fmt -- --check` — verify formatting.
2. `cargo clippy --all-targets` — zero warnings required.
3. `cargo check --all-targets` — zero warnings required.
4. `cargo test` — all tests must pass (if any exist).
5. Review `git diff --stat` and summarize what changed.
6. Suggest an appropriate commit message in conventional format (`fix:`, `feat:`, `docs:`, `refactor:`).
