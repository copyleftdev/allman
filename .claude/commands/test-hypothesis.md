Assert a specific behavioral hypothesis about the Allman codebase through testing.

Hypothesis: $ARGUMENTS

This command enforces TDD discipline. No assumption survives without a test.

## Step 1: Formalize the Hypothesis

Restate the hypothesis as a precise, testable assertion:
- "When [precondition], calling [function/endpoint] with [input] should produce [output/side-effect]."
- Identify the exact code path being tested.
- Identify what WRONG behavior would look like (the null hypothesis).

## Step 2: Write the Test FIRST

1. Create or extend a test file in `src/` (use `#[cfg(test)] mod tests { ... }` in the relevant module, or a standalone integration test in `tests/`).
2. Write the test that will PASS if the hypothesis is true and FAIL if it is false.
3. The test must:
   - Set up realistic preconditions (construct PostOffice state, populate DashMaps, etc.).
   - Exercise the exact code path in question.
   - Assert on the specific outcome — not just "doesn't panic," but the exact return value, exact state mutation, exact error message.
   - Include a doc comment explaining what hypothesis it validates.
4. Run `cargo test` — the test should either pass (confirming hypothesis) or fail (disproving it).

## Step 3: Interpret the Result

- **Test passes**: Hypothesis confirmed. The test is now a regression guard. Commit it.
- **Test fails**: Hypothesis disproved. This is a finding. Investigate:
  - Is this a bug? File it and write a fix on a feature branch.
  - Is this expected behavior that was misunderstood? Update the hypothesis and document the actual behavior.
  - Is the test wrong? Fix the test, not the code.

## Step 4: Commit

- Commit the test with message: `test: assert <hypothesis summary>`
- If a bug was found and fixed, that's a separate commit: `fix: <description>`

Never delete a passing test without explicit direction. Tests are the source of truth.
