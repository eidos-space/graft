# Large-repository SDK benchmark

Measured on 2026-07-30 with an Apple M2, 24 GiB RAM, macOS 15.7.3, Node.js 24.18.1,
and a release-mode Node-API addon. Each distribution contains seven samples. The persistent
fixture has exactly 46,665 tracked paths: 46,318 under `node_modules`, 40 under a nested generated
directory, 305 other tracked files, `.gitignore`, and a nested `.graftignore`. The ignore rules
were committed only after the generated trees were tracked. The history has 51 commits and the
working change is one row inserted into `project.eidos`.

The benchmark source is `benchmark/large-repository.mjs`. A fresh process measures process-cold
status, a new session in the retained benchmark process measures session-cold status, and a warmed
resident session measures hot status. Request and response sizes are compact JSON byte lengths;
RSS is the process high-water mark. The checked-in JSON contains the complete safe telemetry.

## Results

| Operation | Baseline | Final p50 | Final p95 | Budget | Result |
| --- | ---: | ---: | ---: | ---: | --- |
| Process-cold status | 8,655.9 ms | 2,498.8 ms | 2,531.7 ms | reported | — |
| Unchanged hot status | 8,273.7 ms | 101.2 ms | 105.4 ms | <250 ms | pass |
| One changed `.eidos` path diff | 11,261.0 ms | 637.6 ms | 647.9 ms | <1,000 ms | pass |
| 50 history summaries | >60,000 ms for legacy default history | 0.72 ms | 7.56 ms | <500 ms | pass |
| Abort rejection | >60,000 ms / blocked session | 280.0 ms | single cancellation sample | <500 ms | pass |

The history-summary response is 14,260 bytes (budget <1 MiB) and reads zero tree and zero blob
objects. The explicit diff request is 50 bytes and its response is 857 bytes. Status responses are
828 bytes when clean and 1,310 bytes after the database change. For comparison, legacy
`history({limit: 1})` still returns 11,146,927 bytes; callers should migrate list views to
`historySummaries` and hydrate a selected commit with `commitDetails`.

Peak RSS was 186.1 MiB for a process-cold status child, 214.2 MiB for the bounded legacy working
diff child, 240.9 MiB for the cancellation child, and 318.2 MiB for the parent process that ran all
seven-sample distributions. After cancellation, the next incremental status completed in 101.7 ms,
showing that the resident session remained usable. The tracked-and-ignored diagnostic counted
46,358 paths and returned a bounded 100-item page.

## Profiler evidence and changes

The baseline status sample spent its time hydrating the full commit tree, reading every tracked
blob, and repeatedly cloning large `BTreeMap` subtrees. The baseline working diff was dominated by
`repo_worktree_diff_for_filter`, `diff_worktree_artifact`, and `BTreeMap::clone_subtree`, making the
unfiltered path loop effectively quadratic. A correctly ignored 10k-file control repository stayed
at 2–3 ms, confirming that matcher semantics were not the bottleneck.

The implementation therefore caches immutable hydrated commit trees, adds a metadata/generation
status cache, and drives working diff from changed paths. History summaries walk commit objects
without tree/blob hydration, while cooperative cancellation checkpoints cover tree hydration,
status, diff, history, stage, restore, inventory, and SQLite page loops. No budget was relaxed.

The legacy unfiltered working diff also improved from a >60 second timeout to 5.13 seconds, but it
remains a compatibility API. Latency-sensitive hosts should use the bounded changed-path API.
