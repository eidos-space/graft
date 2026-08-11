# Graft SDK performance report

Date: 2026-08-03

Release candidate: `@eidos.space/graft` 0.3.6
Baseline: published `@eidos.space/graft` 0.3.4

## Executive summary

This release is aimed at the application workflow that exposed the bottleneck: opening Version
History and creating a checkpoint in an Eidos Space containing a 460.7 MB SQLite file with a
million-row table. It also adds resumable Remote uploads, but Remote transfer latency is kept out
of the local performance claims below.

The result is not a blanket claim that every operation is faster. The release makes the expensive
SQLite inspection paths incremental and bounded while keeping ordinary Git-like repository work
within benchmark noise:

- On the real 460.7 MB Eidos fixture, dirty status fell from 5.40 s to 19.7 ms, the working table
  summary from 18.84 s to 1.53 s, and the selected metadata-table row page from 10.06 s to 1.69 ms.
- The same fixture's metadata-only checkpoint commit fell from 21.30 s to 1.69 ms. Initial
  stage+commit fell from 10.03 s to 3.78 s.
- Peak RSS on that workflow fell from 3.88 GiB to 672.7 MiB.
- On the paired 5.6 MB Git-like workload, init, stage, commit, row diff, checkout, and filesystem
  push stayed within the harness's noise band. No storage-byte amplification was introduced.
- A known scalability limit remains: `stagePaths` accepts a batch but currently executes each path
  serially. Staging 1,000 changed paths in a 50,000-file worktree takes about 328 s. Applications
  must not put this path on file-open or panel-open critical paths, and a true repository-level
  batch implementation is the next Graft performance priority.

## What changed

The implementation follows the same broad separation that makes Git responsive, adapted to
SQLite rather than pretending a database is an opaque blob:

1. **Cheap classification first.** Repository metadata and file fingerprints determine whether
   cached status is reusable. Remote-tracking projection can refresh without throwing away a
   proven local worktree classification.
2. **Content-addressed derived indexes.** Large SQLite files use checksummed page-hash indexes and
   worktree probes under replaceable cache storage. Cache corruption or a racy fingerprint falls
   back to authoritative comparison.
3. **Stable reads.** Rollback-journal databases are read under a shared SQLite transaction; WAL
   databases use an online-backup snapshot. Repository reads never observe a torn database image.
4. **Stage prepares commit.** Staging retains the exact prepared SQLite snapshot and changed-table
   candidates. Commit consumes that canonical staged image instead of re-reading the live
   worktree.
5. **Layout-specific diff algorithms.** Ordinary rowid tables use the existing changed-page-aware
   diff for checkpoint summaries. Eidos `WITHOUT ROWID` tables use bounded primary-key and
   changed-page traversal. A single algorithm was measurably wrong for one of these layouts.
6. **Hydration proofs stay off the database write set.** Exact snapshot-presence proofs are atomic,
   content-addressed side-cache files. An earlier prototype used an extra Fjall keyspace and made
   ordinary stage/commit 10–20% slower; the paired gate caught it and that design was removed.

This corresponds to Git's index/stat cache, commit-graph, and replaceable derived metadata, but
the cache contents and invalidation rules are Graft-specific. Authoritative snapshots and commits
remain unchanged.

## Test environment and methodology

| Item | Value |
| --- | --- |
| Host | Apple M2, 8 logical CPUs, 24 GiB RAM |
| OS | Darwin 24.6.0, arm64 |
| Local benchmark runtime | Node.js 26.0.0 |
| Release compatibility gate | Node.js 20 and 24 on five native targets |
| Timing | Wall clock via `performance.now()` or paired Rust harness |
| Isolation | Fresh repository and fresh Node child per scale point |
| Fixture generation | Timed separately and excluded from Graft operation timings |
| Git-like comparison | 4 aligned baseline/candidate pairs after 1 warm-up pair |
| Remote used for core comparison | Deterministic filesystem Remote |

Node.js 26 is the locally installed benchmark runtime, not a new support claim. The release
workflow separately builds and runs the SDK contract on Node.js 20 and 24 for macOS arm64/x64,
Linux glibc arm64/x64, and Windows x64.

The synthetic matrix covers:

- 100, 1,000, 10,000, and 50,000 ordinary files, each 256 bytes;
- 1, 100, and 1,000 changed-path rounds where the scale permits;
- SQLite databases with 10,000, 100,000, and 1,000,000 rows;
- 0.75 MB, 27.4 MB, and 410.6 MB SQLite file sizes;
- 1, 100, and 10,000 changed-row rounds;
- cold open/init/stage/commit, resident and reopened status, working and historical summaries,
  first 100 changed rows, incremental stage/commit, history, RSS, and repository bytes.

## Git-like end-to-end workflow

The paired dataset contains a 5.6 MB SQLite database with 20,000 rows, 64 text files, two binary
files, a 10% row update, checkout, and push to a filesystem Remote.

| Operation | 0.3.4 | 0.3.6 candidate | Paired change |
| --- | ---: | ---: | ---: |
| Repository init | 4.22 ms | 4.19 ms | -2.6% |
| Initial stage | 955.88 ms | 977.75 ms | +4.2% (noisy) |
| Initial commit | 520.48 ms | 482.13 ms | -4.0% |
| Stage 10% row update | 539.73 ms | 525.38 ms | -1.5% |
| Incremental commit | 501.34 ms | 541.47 ms | +5.5% |
| Row diff | 557.77 ms | 613.24 ms | +9.5% (high variance) |
| Checkout parent | 589.10 ms | 544.21 ms | -9.2% |
| Filesystem Remote push | 646.71 ms | 657.29 ms | +2.5% |

The comparison marks only incremental commit as a statistically visible small regression. It is
about 40 ms on this dataset and is outweighed in the target Eidos checkpoint path by the prepared
SQLite stage described below. The full aligned samples and median absolute deviations are in
[`results/git-workflow-comparison.md`](results/git-workflow-comparison.md).

Storage was unchanged to the displayed precision: the 9.85 MiB worktree produced a 9.66 MiB
repository after the initial commit and 16.97 MiB after two commits in both versions. The candidate
adds two tiny replaceable cache files in this run; repository objects, snapshot bytes, external
payloads, Remote bytes, and object counts are otherwise unchanged.

## Ordinary-file scale

| Files | Initial stage | Initial commit | Hot clean status p50 | Reopened status | Peak RSS |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 100 | 48.6 ms | 3.45 ms | 1.88 ms | 13.0 ms | 64.7 MiB |
| 1,000 | 216.0 ms | 12.3 ms | 13.1 ms | 59.1 ms | 166.0 MiB |
| 10,000 | 2.17 s | 133 ms | 138 ms | 333 ms | 286.6 MiB |
| 50,000 | 14.82 s | 688 ms | 723 ms | 1.60 s | 680.2 MiB |

The candidate's 50,000-file initial stage is about 12% faster than 0.3.4. Status remains O(number
of visible paths) because Graft must validate fingerprints; it is designed to run after the local
editor is interactive, not before a file can open.

### Explicit path batch limit

| Worktree | 1 changed path | 100 changed paths | 1,000 changed paths |
| ---: | ---: | ---: | ---: |
| 1,000 files | 4.16 ms | 607 ms | 13.68 s |
| 10,000 files | 52.0 ms | 5.84 s | 66.89 s |
| 50,000 files | 268 ms | 31.78 s | 327.96 s |

This is the clearest remaining Graft API problem. The JavaScript call is batched, but the core
implementation still repeats repository work per path. A future `stage_pathset` primitive should
resolve ignore state, index changes, and one repository transaction for the whole set.

## Synthetic SQLite scale

| Rows | SQLite size | Initial stage | Initial commit | Peak RSS |
| ---: | ---: | ---: | ---: | ---: |
| 10,000 | 0.75 MB | 11.7 ms | 8.60 ms | 79.0 MiB |
| 100,000 | 27.4 MB | 86.5 ms | 55.5 ms | 228.5 MiB |
| 1,000,000 | 410.6 MB | 1.10 s | 2.26 s | 770.8 MiB |

For the 410.6 MB database, compared with 0.3.4:

- working summaries improved from 4.89–5.31 s to 3.02–3.65 s;
- the first 100 changed rows improved from 3.66–5.10 s to 0.44–1.73 s;
- incremental stage improved from 1.92–2.16 s to 1.09–1.50 s;
- incremental commit remained 1.23–1.50 s versus 1.27–1.58 s;
- historical summaries improved from 5.18–6.00 s to 3.92–4.09 s.

The candidate uses about 16% more peak RSS on this synthetic rowid fixture (770.8 MiB versus
662.3 MiB) because the faster checkpoint summary decodes rows on changed pages. This is bounded by
changed-page payload, not total-table payload. The real Eidos layout takes the primary-key path and
shows the opposite memory result.

Persistent page-hash caching is enabled only for SQLite files of at least 16 MiB. Below that size,
the fixed cache/index cost was larger than an authoritative scan; small databases therefore bypass
it and keep the simpler path.

## Real Eidos Space fixture

The application fixture is an anonymized `Untitled.eidos` test file, copied read-only into a
temporary repository for each run. Both compared runs used the same 460,689,408-byte source. The
benchmark mutates `eidos__meta`, checkpoints it, then grows one `eidos__views.layout_json` value in
the temporary copy. The second mutation covers overflow-page allocation from a populated freelist
without touching the unrelated million-row table.

| Operation | 0.3.4 | 0.3.6 candidate | Change |
| --- | ---: | ---: | ---: |
| Session open | 464 ms | 501 ms | +8.0% |
| Repository init | 445 ms | 410 ms | -7.8% |
| Initial stage | 6.37 s | 1.80 s | -71.7% |
| Initial commit | 3.66 s | 1.98 s | -46.0% |
| Dirty status | 5.40 s | 19.7 ms | -99.6% |
| Working table summary | 18.84 s | 1.53 s | -91.9% |
| First metadata-table row page | 10.06 s | 1.69 ms | -99.98% |
| Working layout summary | n/a | 1.19 s | new regression case |
| First views-table row page | n/a | 1.83 ms | new regression case |
| Stage metadata change | 6.05 s | 1.58 s | -73.9% |
| Commit metadata change | 21.30 s | 1.69 ms | -99.99% |
| Post-commit status | 7.45 s | 771 ms | -89.7% |
| Historical summary | 12.53 s | 4.92 s | -60.7% |
| Peak RSS | 3.88 GiB | 672.7 MiB | -83.1% |

Session open remains roughly 0.4–0.5 s because it opens the runtime and local stores. Eidos must
therefore keep it off the editor's critical path: local SQLite becomes editable first, Version
History attaches later, and Remote state is requested only when Sync is enabled.

Historical summary is substantially better but still takes seconds on this fixture. It belongs in
a cancellable background request with cached summary rendering; it must not block table switching.

## Remote and Cloudflare scope

The core paired test uses a filesystem Remote to isolate object creation, bundling, ref publication,
and local repository costs from public-network noise. The Remote changes in this release add:

- resumable multipart segment upload negotiation;
- immutable part retries and completion reconciliation;
- Cloudflare R2 multipart storage support;
- compatibility fallback when a Remote does not advertise multipart capabilities.

Protocol and Cloudflare worker tests cover retry, duplicate parts, completion, and fallback. This
report intentionally does not present one staging-network latency sample as a universal Remote
performance number. A separate controlled Cloudflare load test should vary segment size,
concurrency, packet loss, RTT, and interrupted uploads before setting production transfer defaults.

## Application integration rules

The benchmark results imply a strict priority order for Eidos:

1. Open and edit the local Eidos file.
2. Attach the retained Graft repository session in the background.
3. Show cached Version/Sync state immediately, then refresh local status.
4. Load summaries before row payloads; load rows only for the selected table.
5. If Sync is enabled, refresh Remote projection after local state is usable.
6. Cancel stale panel/table requests without surfacing cancellation as an error.

No status, history, Remote, entitlement, quota, or checkpoint request is allowed to gate local
file opening. This is an application scheduling contract in addition to a Graft performance
property.

## Reproduction and raw data

Build the local SDK and run the full matrix:

```sh
pnpm --dir packages/graft-sdk build:native
GRAFT_PERF_PROFILE=full GRAFT_PERF_ITERATIONS=5 \
  GRAFT_PERF_OUTPUT=benchmark/results/performance-matrix-candidate-macos-arm64.json \
  pnpm --dir packages/graft-sdk bench:matrix
```

Run the real Eidos fixture:

```sh
GRAFT_REAL_EIDOS_SOURCE=/absolute/path/to/Untitled.eidos \
  GRAFT_PERF_OUTPUT=benchmark/results/real-eidos-candidate-macos-arm64.json \
  pnpm --dir packages/graft-sdk bench:real-eidos
```

Run the deterministic paired core benchmark with two release binaries:

```sh
cargo build --release --locked -p graft-cli -p graft-bench
./target/release/graft-bench run-paired \
  --baseline-graft-bin /path/to/0.3.4/graft \
  --candidate-graft-bin ./target/release/graft \
  --baseline-output packages/graft-sdk/benchmark/results/git-workflow-v0.3.4.json \
  --candidate-output packages/graft-sdk/benchmark/results/git-workflow-candidate.json \
  --baseline-label graft-sdk-v0.3.4 --candidate-label graft-sdk-v0.3.6 \
  --profile ci --samples 4 --warmups 1
```

Raw results:

- [`performance-matrix-v0.3.4-macos-arm64.json`](results/performance-matrix-v0.3.4-macos-arm64.json)
- [`performance-matrix-candidate-macos-arm64.json`](results/performance-matrix-candidate-macos-arm64.json)
- [`real-eidos-v0.3.4-macos-arm64.json`](results/real-eidos-v0.3.4-macos-arm64.json)
- [`real-eidos-candidate-macos-arm64.json`](results/real-eidos-candidate-macos-arm64.json)
- [`git-workflow-v0.3.4.json`](results/git-workflow-v0.3.4.json)
- [`git-workflow-candidate.json`](results/git-workflow-candidate.json)

## Release gates and next work

This release is acceptable when:

- SDK and SQLite tests pass on the full workspace;
- the paired Git-like workflow has no broad regression;
- the real Eidos fixture keeps dirty status under 100 ms, selected metadata rows under 100 ms,
  and peak RSS under 1 GiB on the reference host;
- all published native packages pass Node.js 20/24 contract tests;
- Remote multipart packages pass typecheck, protocol tests, packaging verification, and a
  Cloudflare dry-run deployment.

Next priorities, in order:

1. implement true core-level batch staging instead of a loop around atomic path operations;
2. persist enough safe table/page change metadata to reduce historical summary below one second;
3. add Linux x64/arm64 benchmark runners and a Windows smoke profile;
4. add controlled Cloudflare multipart throughput and interruption benchmarks;
5. make the benchmark harness record an explicit candidate artifact digest in addition to the
   harness checkout revision.
