# Large-repository SDK benchmark

Measured on 2026-07-30 with an Apple M2, 24 GiB RAM, macOS 15.7.3, Node.js 24.11.1,
and a release-mode Node-API addon. Each distribution contains seven samples. The persistent
fixture has exactly 46,665 tracked paths: 46,318 under `node_modules`, 40 under a nested generated
directory, 305 other tracked files, `.gitignore`, and a nested `.graftignore`. The ignore rules
were committed only after the generated trees were tracked. The history has 51 commits and the
working change is one row inserted into `project.eidos`.

The benchmark source is `benchmark/large-repository.mjs`. It removes only the rebuildable SDK
classification cache, measures one cold build, then opens a new process/session for every warm
reopen sample. A warmed resident session measures hot status. Request and response sizes are
compact JSON byte lengths; RSS is the process high-water mark. The persisted-classification run is
checked in as `persistent-classification-macos-arm64.json`.

## Results

| Operation | Baseline | Final p50 | Final p95 | Budget | Result |
| --- | ---: | ---: | ---: | ---: | --- |
| Process-cold status | 8,655.9 ms | 2,505.8 ms | 4,343.9 ms | reported | — |
| Cold persistent snapshot build | 12,690 ms real-repo baseline | 4,647.7 ms single sample | — | reported | — |
| New-process snapshot status | 12,690 ms real-repo baseline | 340.4 ms | 362.6 ms | <1,000 ms | pass |
| New-process open + first status | 12,690 ms real-repo baseline | 715.5 ms | 749.0 ms | <1,000 ms | pass |
| Unchanged hot status | 8,273.7 ms | 103.4 ms | 113.5 ms | <250 ms | pass |
| 1,000-path ignore batch | >30,000 ms with serialized single-path calls | 2.4 ms | 2.6 ms | <100 ms hot | pass |
| First tracked-and-ignored page after status | 7,570.0 ms | 23.7 ms | single first sample | <1,000 ms | pass |
| Hot tracked-and-ignored page | 7,570.0 ms | 8.3 ms | 9.3 ms | <100 ms | pass |
| New-session tiny-path diff, no prior status | 7,835.3 ms first / 1,971.5 ms hot | 0.47 ms | 29.1 ms including first call | <1,000 ms | pass |
| One changed `.eidos` path diff after status | 11,261.0 ms | 2.9 ms | 3.8 ms | <1,000 ms | pass |
| 50 history summaries | >60,000 ms for legacy default history | 0.76 ms | 7.67 ms | <500 ms | pass |
| Abort rejection | >60,000 ms / blocked session | 301.7 ms | single cancellation sample | <500 ms | pass |

The history-summary response is 14,260 bytes (budget <1 MiB) and reads zero tree and zero blob
objects. The no-prior-status tiny-path request is 49 bytes and its response is 474 bytes; the
`.eidos` request is 50 bytes and its response is 913 bytes. Status responses are 828 bytes when
clean and 1,310 bytes after the database change. For comparison, legacy
`history({limit: 1})` still returns 11,146,927 bytes; callers should migrate list views to
`historySummaries` and hydrate a selected commit with `commitDetails`.

The 1,000-path ignore request is 38,987 bytes and its response is 137,096 bytes. The five-item
tracked-and-ignored inventory page is 851 bytes. Its first call after resident status examines all
46,665 tracked paths once; later pages report `inventory_cache_hit: true` and `paths_examined: 0`.
Batch results distinguish a physical or index-derived directory and report whether it has tracked
descendants, so an ignored directory containing indexed files is not incorrectly pruned.

Peak RSS was 198.7 MiB for a process-cold status child, 212.0 MiB for the bounded legacy working
diff child, 251.8 MiB for the cancellation child, and 363.0 MiB for the parent process that ran all
seven-sample distributions. After cancellation, the next incremental status completed in 104.9 ms,
showing that the resident session remained usable. The tracked-and-ignored diagnostic counted
46,358 paths and returned a bounded five-item page.

## Profiler evidence and changes

The baseline status sample spent its time hydrating the full commit tree, reading every tracked
blob, and repeatedly cloning large `BTreeMap` subtrees. The baseline working diff was dominated by
`repo_worktree_diff_for_filter`, `diff_worktree_artifact`, and `BTreeMap::clone_subtree`, making the
unfiltered path loop effectively quadratic. A correctly ignored 10k-file control repository stayed
at 2–3 ms, confirming that matcher semantics were not the bottleneck.

The implementation therefore caches immutable commit trees, adds a metadata/generation status
cache, and drives working diff from changed paths. For an explicit path, `diffPaths` now binary
searches the requested tree/index entry and reads only the referenced blob. It does not hydrate or
clone the complete 46,665-path maps. A second hidden full scan was removed from result rendering:
reading the current revision and branch now resolves the HEAD reference directly instead of calling
full `status`. Telemetry reports `path_filter_fast_path: true` and
`full_tree_paths_hydrated: 0`. History summaries walk commit objects
without tree/blob hydration, while cooperative cancellation checkpoints cover tree hydration,
status, diff, history, stage, restore, inventory, and SQLite page loops. No budget was relaxed.

The same fix was gated read-only on the real 46,665-path Eidos repository. `.DS_Store` improved
from 7,835 ms cold and 1,972–2,139 ms hot to 26.0 ms cold and 0.24–0.32 ms hot. A 3 KiB README
took 0.55 ms. The 336 KiB `.eidos` file took 39.0 ms cold and 9.8–10.0 ms hot, with 7.4 KiB
responses. These calls used one newly opened session and did not run status first.

The classification cache now survives `RepositorySession` close/open and utility restart. Its
content-addressed snapshot is keyed and validated by repository/schema format, HEAD, index,
refs/config, relevant ignore-source contents, and current path metadata. The 46,665-path snapshot
is 22,427,846 bytes; status responses remain 1,077–1,080 bytes. An atomic write uses a synced
same-directory temporary file followed by rename, so a strong kill can leave only an ignored temp
file. Corrupt, truncated, mismatched, or stale candidates trigger a full rebuild.

On this fixture the cold build took 4.65 seconds. A fresh process then returned first status in
297–363 ms (p95 363 ms), or 696–749 ms including open (p95 749 ms). Resident hot status remained
99.0 ms p50 / 106.6 ms p95. `repositoryMetadata` measured 0.164/0.340 ms p50/p95 and
`listRemotes` 0.061/0.157 ms; both reported `paths_examined: 0`. No budget was relaxed.

For ignore-heavy traversal, the retained session now also caches the tracked index, compiled root
and nested ignore matchers, and the complete tracked-and-ignored classification. Matcher evaluation
borrows compiled rules instead of cloning them for every tracked path. Source fingerprints
invalidate loaded `.gitignore` and `.graftignore` rules, while HEAD/index comparison invalidates
inventory after an external writer. Telemetry exposes only duration, counts, and cache-hit booleans.

The legacy unfiltered working diff also improved from a >60 second timeout to 3.46 seconds, but it
remains a compatibility API. Latency-sensitive hosts should use the bounded changed-path API.
