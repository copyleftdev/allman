Open a pull request for the current feature branch.

## Pre-flight (mandatory)

1. `git branch --show-current` — must NOT be `master`. Abort if on master.
2. `cargo fmt -- --check` — must pass.
3. `cargo clippy --all-targets` — zero warnings.
4. `cargo check --all-targets` — zero warnings.
5. `cargo test` — all tests pass.
6. `git log --oneline master..HEAD` — summarize all commits on this branch.

## Build the PR Description

Structure the PR body as:

```
## Summary
One paragraph: what this branch does and why.

## Changes
- Bulleted list of each commit with a one-line explanation.

## Testing
- List every test added or modified.
- Include benchmark comparison if performance-relevant (before/after table).
- State: "All existing tests pass" with the `cargo test` output.

## Hypotheses Validated
- List each hypothesis that was tested and whether it was confirmed or disproved.

## Risk Assessment
- What could go wrong with this change?
- What failure modes were considered?
- What is the rollback plan? (Usually: revert the merge commit.)

## Checklist
- [ ] `cargo fmt` clean
- [ ] `cargo clippy` zero warnings
- [ ] `cargo check --all-targets` zero warnings
- [ ] `cargo test` all pass
- [ ] No new dependencies without justification
- [ ] CHANGELOG.md updated (if user-facing change)
- [ ] README.md updated (if API surface changed)
```

## Create the PR

1. Push the branch: `git push -u origin $(git branch --show-current)`
2. Open the PR: `gh pr create --base master --title "<type>: <summary>" --body "<structured body above>"`
3. Report the PR URL.

Do NOT merge. The PR is for review.
