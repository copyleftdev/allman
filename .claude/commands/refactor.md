Refactor $ARGUMENTS (file, module, or function) using a test-first, hypothesis-driven approach.

No refactoring without tests. No assumptions — only assertions.

## Phase 1: Characterize Current Behavior

1. Read the target code. Document every observable behavior:
   - Inputs → outputs for each public function.
   - Side effects (DashMap mutations, crossbeam sends, Tantivy writes, Git commits).
   - Error conditions and what they return.
   - Edge cases: empty inputs, missing keys, concurrent access patterns.
2. Write characterization tests that capture EVERY behavior listed above.
   - These tests must pass on the CURRENT code before any refactoring begins.
   - Name them `test_<function>_<scenario>` (e.g., `test_get_inbox_empty_agent`, `test_send_message_single_recipient`).
3. Run `cargo test` — all characterization tests must pass. This is the contract.

## Phase 2: Plan the Refactoring

1. State the goal: What specific quality is being improved? (readability, separation of concerns, testability, performance, reduced duplication)
2. State what must NOT change: the behavioral contract from Phase 1.
3. Identify risks: Which refactoring steps could accidentally change behavior?
4. Break the refactoring into the smallest possible atomic steps.

## Phase 3: Execute on a Feature Branch

1. `git checkout -b refactor/<target>`
2. For EACH atomic step:
   a. Make the change.
   b. `cargo check --all-targets` — zero warnings.
   c. `cargo test` — ALL characterization tests still pass.
   d. Commit with a descriptive message.
3. If any characterization test fails, STOP. Either the refactoring introduced a regression (revert) or the test was wrong (investigate and fix the test FIRST, on master, before continuing).

## Phase 4: Strengthen

1. After refactoring, add any NEW tests that the cleaner structure makes possible.
2. Run the full benchmark suite to verify no performance regression.
3. `cargo clippy --all-targets` — zero warnings.

## Phase 5: PR

1. Open PR with:
   - **Goal**: What was improved.
   - **Behavioral contract**: List of characterization tests proving no regression.
   - **New tests**: Any additional coverage added.
   - **Benchmark comparison**: Before/after if applicable.
