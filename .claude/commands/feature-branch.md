Start a new feature branch with proper Git flow. Feature name: $ARGUMENTS

## Step 1: Ensure Clean State

1. `git status` — working tree must be clean. If dirty, stop and ask what to do with uncommitted changes.
2. `git checkout master && git pull origin master` — sync with remote.

## Step 2: Create Branch

1. `git checkout -b feature/$ARGUMENTS`
2. Confirm: `git branch --show-current` should print `feature/$ARGUMENTS`.

Branch naming conventions:
- Features: `feature/<name>`
- Fixes: `fix/<name>`
- Refactors: `refactor/<name>`
- Optimizations: `optimize/<name>`
- Tests: `test/<name>`
- Docs: `docs/<name>`

## Step 3: Work Rules

While on a feature branch:
- Commit early and often. Each commit must be atomic and pass `cargo check --all-targets`.
- Write or update tests BEFORE implementation (TDD).
- Never push directly to master. All work merges via PR.
- If master moves ahead, rebase: `git fetch origin && git rebase origin/master`

## Step 4: When Ready

Run `/preflight` before opening a PR. Then run `/pr` to create the pull request.
