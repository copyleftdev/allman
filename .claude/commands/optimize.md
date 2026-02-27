Optimize the performance of $ARGUMENTS (file path, function name, or "hot-path" for the full critical path).

Every optimization must be proven, not assumed.

## Phase 1: Baseline

1. Ensure the server is running: `curl -sf http://localhost:8000/health`
2. Run the benchmark suite and record baseline numbers:
   - `cargo run --release --bin benchmark` → capture registration, ingestion, search, inbox rates.
   - `cargo run --release --bin swarm_stress` → capture all 4 phase rates.
3. Store these numbers. They are the control group.

## Phase 2: Profile and Hypothesize

1. Read the target code. Identify:
   - Heap allocations on the hot path (String::from, Vec::new, .clone(), .to_string(), format!()).
   - Lock contention points (DashMap entry API holds a shard lock during the closure).
   - Unnecessary serialization/deserialization cycles.
   - Synchronous I/O accidentally on an async task.
   - Redundant data copies between layers.
2. For each finding, state a hypothesis: "Removing [X] will improve [metric] by approximately [estimate] because [reason]."

## Phase 3: Implement on a Feature Branch

1. Create a feature branch: `git checkout -b optimize/<target>`
2. Implement ONE optimization at a time. Small, isolated commits.
3. After each change: `cargo check --all-targets` — zero warnings.

## Phase 4: Measure

1. Run the exact same benchmark suite from Phase 1.
2. Compare results to baseline. For each optimization:
   - Did the metric improve? By how much?
   - Did any OTHER metric regress? (Optimizations that rob Peter to pay Paul are rejected.)
3. If improvement is < 2%, the optimization is noise — revert it unless it also improves readability.

## Phase 5: Test

1. All existing tests must still pass: `cargo test`
2. Add a targeted test that exercises the optimized path under concurrent load.
3. The test must assert correctness, not just "doesn't panic."

## Phase 6: Report

```
## Optimization Report: [target]

### Baseline
| Metric | Before |
|--------|--------|
| ...    | ...    |

### Changes Applied
| Commit | Description | Metric | Before | After | Delta |
|--------|-------------|--------|--------|-------|-------|
| ...    | ...         | ...    | ...    | ...   | ...   |

### Rejected (reverted)
| Description | Reason |

### Tests Added
| Test | What it asserts |
```

Open a PR with this report as the description.
