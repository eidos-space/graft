# Native path history and single-file restore — 2026-09-07

Local integration candidate: `@eidos.space/graft@0.3.26-rc.0`, branch
`codex/path-history-native`. This candidate is **unpublished**. Formal SDK release
must wait for an explicit Eidos integration-success message.

## Contract

```js
const page = await session.pathHistory({
  path: 'notes/example.md',
  limit: 50,          // 1..100 matches
  maxCommits: 100,    // 1..1000 comparisons
  maxBytes: 8388608,  // 1024..67108864 actual object bytes
  // cursor: previous.next_cursor,
  // signal: abortController.signal,
})
```

The full normative contract is [repository §8.4](../../../../docs/specs/graft-repository-1.0.md#84-bounded-exact-path-history-sdk-extension).
Results contain `path`, pinned `start`, `commits`, `has_more`, `next_cursor` and
read telemetry. Each entry includes `id`, all `parents`, `message`, `timestamp_ms`
and `change: added|modified|deleted`. Use `id` as `readPathContent.revision`.
A deletion reads as absent; `parents[0]` identifies its previous version.

- Exact path, first-parent traversal; timestamps do not determine ordering.
  Merge entries compare against the first parent and do not enumerate the
  second-parent chain. Renames are old-path deletion/new-path addition; no
  rename following. Deletion/recreation stays in the same path history.
- Cursor contains a version, normalized path, initial HEAD and next unscanned
  commit, plus a corruption checksum. Different paths and damaged cursors fail.
  It is opaque to consumers, not an authenticated security token. Continuation
  never rescans from HEAD and is unaffected by new commits, rewinds or branch
  switches. Unreachable starting commits remain usable while required objects
  exist; cursors do not pin objects against GC. Missing/corrupt objects fail.
- Empty `commits` with `has_more:true` is a valid scan page. Only
  `has_more:false` means exhaustion. The frontend must not automatically loop
  indefinitely to fill a results page. Budget/limit stops resume at the next
  unscanned commit. If zero comparisons fit, an explicit invalid-argument error
  asks for more bytes; even 64 MiB may be insufficient for an exceptional object.
- Counters include actual canonical commit/tree bytes read, even work toward an
  unfinished final comparison. A page has at most one unfinished comparison.
  No blob payload reads, worktree materialization, or persistent history index.
  A single selected tree identity is retained, not full trees. Cancellation
  checkpoints occur per comparison, per 64 KiB read, and around decoding. One
  bounded object decode is not internally interruptible.

## Measurements

Native release build, macOS 15.7.3 arm64, Node 24.20.0, Rust 1.91.1. Raw data:
[path-history-macos-arm64.json](./path-history-macos-arm64.json). The first sample
uses a fresh Node process/session; the OS file cache is **not flushed**. These
are session-cold and repeated-query measurements, not cold-disk measurements.
Five repeated samples are reported as the median. Each rare path changes only
in the root commit, so a returned-match count of zero cannot hide scan cost.

| Scenario | First / warm median | Comparisons/page | Commit/tree reads | Actual bytes/page | Blob reads |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1,200 commits, 33 paths | 9.15 / 5.32 ms | 100 | 200 / 101 | 365,992 | 0 |
| 100 commits, 10,001 paths | 33.52 / 25.21 ms | 8 | 18 / 9 | 7,738,578 | 0 |

Both first pages return zero matches and a continuation. The large directory
stops on the byte budget before reaching its comparison limit. Traversing the
entire rare-path history is deliberately done only by the benchmark, with a
separate page-count guard:

| Scenario | Full traversal | Pages / comparisons / matches | Total bytes | Query process peak RSS | Restore latency |
| --- | ---: | ---: | ---: | ---: | ---: |
| Long history | 81.24 ms | 12 / 1,200 / 1 | 4,386,316 | 52.1 MiB | 2.02 ms |
| Large directory | 416.17 ms | 13 / 100 / 1 | 96,301,714 | 74.4 MiB | 216.49 ms |

Initial process RSS was approximately 51.1/50.7 MiB. Peak after subsequent
staging/diff/restore validation was 54.3/105.8 MiB. RSS is whole-process high-water
memory, not an allocation count for a single call. Aborting after a 2 ms timer
rejected the promise in 2.76/2.94 ms; a subsequent one-comparison query completed
in another 0.14/6.14 ms, including native-worker unwind/queue recovery.

The main history cost is reading, hashing and decoding flat trees, proportional
to metadata bytes. Bounded pages stay responsive in these fixtures; they do not
make whole-history scans constant time. No Bloom filter or rebuildable index is
justified for this first integration based on these measurements. Reassess for
larger combined history/directory workloads or repeated full-history demand.
The current restore planner still loads whole source/index/HEAD maps and related
artifact metadata; its roughly 216 ms large-directory cost is a separate
bottleneck. This change does not claim to optimize that existing restore path.

## Restore evidence and safe integration

Real Node SDK tests restore one file while both that file and an unrelated file
have staged versions and later external changes. HEAD and the **entire index**
remain unchanged. Only the target worktree contents change; unrelated dirty and
untracked files survive. A deletion version removes only the selected file.
`requireClean:true` rejects work anywhere in the tracked repository and leaves
the target unchanged. `expectedHead` mismatch also rejects before replacement.
With `requireClean:false`, a matching HEAD still permits overwriting externally
modified target contents: this limitation is tested, not inferred from the name.

Eidos must capture the reviewed target content/existence and staging state,
coordinate all its writers, recheck those states immediately before restore, and
keep writers paused until completion. Draft handling and copying belong to the
host. A precheck without writer coordination has a TOCTOU window; Graft does not
provide atomic content-CAS against arbitrary external writers. File replacement
is an atomic rename, but following bookkeeping may fail after replacement.
Multi-path SDK calls run normalized, sorted paths sequentially: a later invalid
path leaves earlier replacements applied (also tested). Re-read actual state
following any mutation error/cancellation before retrying. Pass an exact file
path; directory pathspecs can affect descendants.

## Verification and reproduction

Final source/native validation:

- `cargo nextest run -p graft -p graft-sdk -p graft-sqlite`: **426 passed**.
- `cargo test --doc -p graft -p graft-sdk -p graft-sqlite`: **1 passed**.
- `cargo clippy --workspace --all-targets --exclude fjall --no-deps` and
  `cargo fmt --all --check`: passed; only existing vendored compiler warnings.
- `node --test packages/graft-sdk/test/*.test.js`: **29 passed**, including four
  new native history/restore tests and a real remote three-way merge.
- Core fixtures omit content blobs entirely, proving metadata-only operation;
  they exercise time-inverted first-parent traversal, unreachable pinned HEAD,
  byte limits, sparse scans, oversized-comparison failure and cancellation.
- Local main/native tarballs install **offline**; installed package smoke opens
  a repository, commits a file and queries `pathHistory`. JS metadata, native
  package metadata, native `sdkVersion()`, both Rust SDK crates and Cargo.lock
  agree on `0.3.26-rc.0`.

HTTP tests require permission to bind localhost. Sandbox-only runs failed at
that boundary; the full permission-enabled runs above passed. Only darwin-arm64
native artifacts were built locally. Other native targets and the five-target
release assembly gate must run in release CI; no cross-platform result is claimed.

```sh
bash packages/graft-sdk/scripts/build-native.sh
node --test packages/graft-sdk/test/*.test.js
node packages/graft-sdk/benchmark/path-history.mjs
node packages/graft-sdk/scripts/pack-local.mjs
```

The benchmark defaults to the two scenarios above. Set
`GRAFT_HISTORY_COMMITS`, `GRAFT_HISTORY_LARGE_COMMITS`, and `GRAFT_HISTORY_PATHS`
to explore other workloads; fixture creation uses the real SDK and is timed
separately. It creates and removes a private temporary fixture.

Local output directory: `target/local-sdk/0.3.26-rc.0/`:

- `eidos.space-graft-0.3.26-rc.0.tgz`
- `eidos.space-graft-darwin-arm64-0.3.26-rc.0.tgz`
- `verification/`: final Rust/Node/clippy/doctest/build logs.

Install both tarballs together. The verified disposable installation used:

```sh
npm install --offline --ignore-scripts --omit=optional --no-audit --no-fund \
  /absolute/path/eidos.space-graft-0.3.26-rc.0.tgz \
  /absolute/path/eidos.space-graft-darwin-arm64-0.3.26-rc.0.tgz
```

Alternatively require the source SDK directory; its loader finds
`packages/graft-sdk/native/graft-sdk.darwin-arm64.node`. An explicit
`GRAFT_SDK_NATIVE_PATH` may point to that binary. Do not pair the new JS API with
an installed 0.3.25 native binding. The generated root tarball pins all optional
native package versions to the same rc; only the host-native tarball is supplied
for local testing. Nothing was pushed, tagged, or published.
