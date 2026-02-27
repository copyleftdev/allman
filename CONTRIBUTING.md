# Contributing to Allman

Contributions are welcome. This document covers the process and standards.

## Getting Started

```bash
git clone https://github.com/copyleftdev/allman.git
cd allman
cargo build
cargo check --all-targets
```

## Development Workflow

1. **Fork** the repository and create a feature branch from `master`.
2. **Keep changes small and focused.** One logical change per pull request.
3. **Run checks before submitting:**

```bash
cargo fmt
cargo check --all-targets
cargo test
```

4. **Open a pull request** with a clear description of what changed and why.

## Code Standards

- Run `cargo fmt` on all Rust files.
- `cargo check --all-targets` must pass with **zero warnings**.
- No new dependencies without justification in the PR description.
- Follow existing code style and comment conventions.
- Hot-path code must avoid disk I/O and heap allocation where possible.

## Commit Messages

Use concise, imperative-mood subjects:

```
fix: resolve inbox drain race on empty DashMap entry
feat: add batch broadcast to send_message
docs: update MCP tools table in README
```

## Reporting Bugs

Open a GitHub issue with:

- Steps to reproduce
- Expected vs. actual behavior
- Server logs (set `RUST_LOG=allman=debug`)

## Security Issues

Do **not** open public issues for security vulnerabilities. See [SECURITY.md](SECURITY.md).
