Perform a deep code review of $ARGUMENTS (file path, module, or "all" for the full codebase).

This is NOT a surface-level lint pass. This is adversarial, hypothesis-driven review.

## Phase 1: Read and Map

1. Read every file in scope. Build a mental model of data flow, ownership, and error paths.
2. Identify all public interfaces, invariants, and implicit assumptions.

## Phase 2: Hypothesize Failure Modes

For each file, generate specific hypotheses about potential failures:

- **Concurrency**: Can two requests race on the same DashMap key? Can `get_inbox` drain interleave with `send_message` append?
- **Memory**: Can unbounded inbox growth exhaust memory? Are there DashMap entries that are never cleaned up?
- **Error propagation**: Which `unwrap()` calls can actually panic in production? Are `try_send` failures silently dropped?
- **Correctness**: Does `search_messages` ever return stale data? Can `create_agent` silently overwrite a different agent's data?
- **Resource leaks**: Are Tantivy index writers / Git repo handles properly cleaned up on shutdown?
- **Input validation**: What happens with empty strings, null bytes, extremely long payloads, or malformed JSON?

For each hypothesis, state it as: "I hypothesize that [specific scenario] will cause [specific failure]."

## Phase 3: Prove or Disprove

For each hypothesis:

1. Trace the code path to determine if the failure is possible.
2. If provable, write a concrete test case that demonstrates the failure.
3. If disprovable, write a test case that asserts the correct behavior (regression guard).
4. If indeterminate, flag it with severity and recommend investigation.

## Phase 4: Report

Output a structured report:

```
## Deep Review: [scope]

### Confirmed Issues
- [H1] Description | Severity | File:Line | Proposed fix

### Disproved Hypotheses (regression tests added)
- [H2] Description | Test name | Why it's safe

### Open Questions
- [H3] Description | Severity | What's needed to resolve

### Test Coverage Added
- test_name_1: What it asserts
- test_name_2: What it asserts
```

Do NOT make any code changes during the review. Only report and write tests.
