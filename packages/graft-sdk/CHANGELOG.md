# Graft SDK changelog

This changelog covers the independently versioned `@eidos.space/graft` package. Core Graft CLI and
SQLite extension releases are documented in the repository-level `CHANGELOG.md`.

## Unreleased

### Fixed

- SQLite worktree comparisons now ignore the schema cookie rewritten by online backup, preventing
  clean databases committed on macOS from appearing modified after a Windows clone.

### Compatibility

- The JavaScript API, repository, and snapshot formats are unchanged. Disposable SQLite page-index
  and worktree-probe caches are rebuilt under version 5.

## Graft SDK 0.3.21 — 2026-08-26

### Changed

- Owner-published incremental status snapshots can now be revalidated without opening runtime
  storage, allowing the matching Graft CLI to serve exact `status` results while an Electron or
  Node.js application retains the repository lock.
- Repository index updates now use atomic replacement so lock-independent readers observe a
  complete old or new index.

### Compatibility

- The JavaScript API and repository/status-cache formats are unchanged. Existing SDK clients do
  not require code changes.

## Graft SDK 0.3.20 — 2026-08-25

### Fixed

- Incremental SQLite capture deltas now apply reliably to exported WAL-mode base snapshots because
  Graft preserves their exact page-1 bytes and SHA-256 identity.

## Graft SDK 0.3.19 — 2026-08-25

### Added

- `captureSqliteSnapshot()` exports a consistent create-new SQLite image without changing Graft
  history or the worktree. A prior opaque capture token enables page reuse and an optional bounded
  `GRAFTD01` fixed-page delta for content-addressed publication transports. Capture results now
  expose the exact snapshot SHA-256; delta results expose their embedded base and target digests.

## Graft SDK 0.3.18 — 2026-08-17

### Changed

- Fetch records commit ancestry in new object-pack indexes and uses it to prefetch the complete
  pack chain from the requested head to a local ancestor in one bounded read bundle. The retained
  eight-push plus unrelated-branch regression collapses the cold Fetch topology from 19 HTTP
  requests to 4 while excluding the unrelated pack. Pre-existing indexes receive a bounded
  128-pack/48 MiB migration path before falling back to exact lazy reads.
- Fetch coalesces immutable pack-index reads and missing SQLite snapshot commits into bounded
  Remote read bundles. In the retained Windows trace with 15 pack indexes, the index phase changes
  from 15 authenticated HTTP GETs to one request; longer histories are split automatically at the
  service limit.
- A completed fast-forward pull no longer performs an immediate full worktree status scan merely
  to confirm that no merge remains. The next explicit status request remains authoritative and
  still detects concurrent external writes.
- Pack indexes are cached locally as repairable performance hints, so unchanged immutable indexes
  are not downloaded again after reopening the application.
- macOS and Windows row auto-merge can seed the first private candidate from the Ours worktree
  instead of reconstructing the complete SQLite file from page storage. The private copy must
  match Ours' exact content-addressed page index before use; a missing index, changed worktree, or
  copy mismatch retains authoritative snapshot materialization.
- Verified candidate construction and the complete immutable-Ours integrity check now overlap row
  merge analysis. Unused candidates are cancelled and removed, while a required candidate is not
  published until exact page-index verification and the full check both succeed.
- New local storage commits retain the hash computed from their already-resident segment pages;
  legacy commits are backfilled after their first hash calculation. Sparse merge imports no longer
  rescan a historical full-database segment to rebuild hashes that were already established.
- SQLite merge planning reuses one dense page-origin manifest per Base/Ours/Theirs snapshot across
  candidate selection and parallel row diffs instead of rebuilding ordered maps for each pass.

### Compatibility

- The optional `commits` field in pack-index format version 1 is an integrity-neutral hint. Older
  clients ignore it, while new clients validate fetched object bytes exactly as before.
- Repository and snapshot formats are unchanged. SDK clients fall back to bounded individual reads
  against Remote services that do not advertise or accept `read-bundle`.

## Graft SDK 0.3.17 — 2026-08-17

### Fixed

- Merge completion now resolves and verifies every staged SQLite snapshot from the configured
  Remote before recording the two-parent commit, preventing an incomplete local snapshot from
  becoming history that cannot be pushed.
- Push preflight now hydrates referenced storage commits from the destination Remote when the local
  repository no longer has them. Valid Remote-backed history can recover and publish instead of
  entering a repeated missing-storage failure loop.

### Compatibility

- Repository, snapshot, merge, and Remote formats are unchanged. If the referenced storage is
  absent both locally and remotely, the operation still fails without publishing partial history.

## Graft SDK 0.3.16 — 2026-08-16

### Fixed

- `applyMerge()` now accepts the same optional transfer-progress callback as fetch and pull. Hosts
  can fetch once, plan against the Remote-tracking ref, and display any snapshot hydration bytes
  while atomically applying that immutable plan without a redundant Remote-ref fetch.

## Graft SDK 0.3.15 — 2026-08-15

### Added

- `repositoryMetadata()` now reports the last locally known upstream target so history UIs can
  decorate a diverged Remote tip without fetching or scanning the worktree.

### Changed

- SQLite merge preparation reuses proven segment page sets across immutable snapshots and targets
  `WITHOUT ROWID` analysis to changed B-tree pages instead of repeatedly materializing complete
  database pairs. Candidate construction also establishes one full integrity proof for an exact
  immutable Ours state before mutation, then validates SQLite's transactional delta instead of
  cold-scanning every inherited page again. On the retained macOS 417 MiB Eidos fixture, warmed
  merge lifecycle P95 fell from 42.34 seconds to 1.56 seconds; a fresh process completed in 2.93
  seconds.
- On macOS, an already-proven, clean, exclusively locked Ours worktree seeds private merge
  candidates with an APFS copy-on-write clone. Unsupported filesystems and other platforms retain
  the authoritative snapshot-materialization fallback.
- Transactional SQLite merge replay now carries committed WAL page numbers into repository import,
  so validated sparse changes avoid rereading the complete candidate. Missing, malformed, or
  partial WAL data retains the authoritative full-import fallback.
- A validated final SQLite merge candidate is installed directly and exact-token
  `continueMerge()` commits that staged state without a second database rewrite. When completion
  does not physically change a path, its exact `worktree_paths` result is empty. The SDK also
  carries validated file fingerprints across that ref/index-only commit, avoiding an immediate
  full status scan while preserving stat-based invalidation for external writes.

### Fixed

- Multi-request pushes now declare their complete known upload payload before transfer starts.
  Progress no longer reports each completed request as a misleading new `100%` total; fallback
  retries add their remaining payload as one planned unit.
- Progress callbacks are rate-limited across short request bodies, and the JavaScript operation
  waits one event-loop turn before settling so hosts receive the exact final byte count without a
  large callback backlog making a completed push appear stuck.

### Compatibility

- `upstream_target` is additive and nullable. Repository, snapshot, merge, and Remote formats are
  unchanged. `operationMaterializesWorktree("continueMerge")` remains a conservative `true` host
  gate even when the validated completion fast path returns no changed worktree paths.

## Graft SDK 0.3.14 — 2026-08-13

### Changed

- Clone transfer progress now uses the exact framed upload-bundle size declared by newer Graft
  Remote services, allowing hosts to show total download size, percentage, and estimated time
  remaining from the start of the streamed response.

### Compatibility

- Older Remote services remain supported through their HTTP `Content-Length`. New clients reject a
  response only when the Graft total-size header and `Content-Length` are both present but disagree.

## Graft SDK 0.3.13 — 2026-08-13

### Added

- Added a generic, durable semantic-merge provider handoff for application-owned SQLite rules.
  Providers receive immutable Base/Ours/Theirs snapshots plus a private Ours-derived candidate
  containing safe non-managed Theirs changes, and may persist bounded domain conflicts or accept a
  validated result under exact provider and merge-state tokens.

### Fixed

- Semantic result acceptance now records its validation audit before worktree materialization, so a
  process interruption remains retryable instead of leaving an apparently failed but already staged
  path.

### Compatibility

- Repository, snapshot, merge-journal, and Remote formats are unchanged. The provider APIs are
  additive, provider workspaces stay private under `.graft`, and Node.js 20 remains the minimum
  supported runtime.

## Graft SDK 0.3.12 — 2026-08-13

### Changed

- Row, cell, and table conflict choices now update the durable resolution journal without
  rebuilding the SQLite candidate until the path is complete, and retained sessions reuse the
  immutable merge plan between choices.
- Merge operations report only the paths actually materialized, allowing hosts to skip redundant
  validation and reopen work after journal-only choices.

### Fixed

- Resolving SQLite conflicts entirely to the existing Ours result now reuses the parent snapshot
  instead of recording a physical-only merge modification with no schema or row changes.

### Compatibility

- Existing repository, snapshot, merge-journal, and Remote formats are unchanged. The new
  `worktree_paths` fields are additive, and Node.js 20 remains the minimum supported runtime.

## Graft SDK 0.3.11 — 2026-08-13

### Added

- Added real HTTP upload/download byte progress for `push`, `fetch`, `pull`, and
  `cloneRepository`, including optional totals and rate-limited final events for indeterminate
  response bodies.

### Fixed

- Freshly cloned SQLite worktrees no longer appear modified when the source and destination were
  written by different SQLite library versions; volatile page-one header metadata is ignored while
  actual database-content changes remain detectable.

### Compatibility

- Existing repository and Remote formats are unchanged. Transfer progress callbacks are optional
  and additive, and Node.js 20 remains the minimum supported runtime.

## Graft SDK 0.3.10 — 2026-08-12

### Added

- Added versioned, compare-and-swap merge policies with explicit same-row merging, semantic-key
  collations, and data-only managed-column resolvers. Plans bind the exact policy token and active
  merges freeze that policy until continue or abort.
- Added durable cell and safe table resolution, reversible path resolution, and validated staging
  of an application-owned SQLite result. Every new asynchronous SDK operation accepts an
  `AbortSignal`.
- Added directory-wide multi-SQLite schema union, validated table rebuilds, SQLite internal-state
  resolvers, and structured diagnostics for skipped, corrupt, or analysis-failed tracked paths.

### Changed

- Merge inspection now retains resolved and unresolved conflict detail across session reopen and
  reports structured cell values, resolution state, recommendations, and current policy metadata.
- Worktree materialization declarations now cover every merge apply, resolution, unresolve,
  edited-text, continue, and abort operation so hosts can close and restore live SQLite handles.

### Fixed

- Compatible column, table, index, view, and trigger changes now form one validated result across
  every SQLite database in a directory-backed repository.
- Complete-file table/view collision choices no longer read the wrong object as a table B-tree, and
  malformed tracked SQLite files remain dirty with a precise non-destructive diagnostic.

### Compatibility

- Existing repository and Remote object formats are unchanged. Policy v1 defaults
  `same_row_merge` to false, so applications must opt in explicitly for formats that support it.
- Policy resolvers remain generic and data-only; application-specific recomputation and domain
  validation stay with the host before `stageMergeSqliteResult`.
- Node.js 20 remains the minimum supported runtime.

## Graft SDK 0.3.9 — 2026-08-11

### Added

- Added a durable merge workflow spanning `fetch`, `planMerge`, `applyMerge`, merge status and path
  inspection, bounded base/ours/theirs/result reads, text editing, whole-path selection, SQLite row
  resolution, `continueMerge`, and `abortMerge`.
- Added immutable plan and state tokens so host applications can reject stale merge plans, changed
  HEADs, concurrent writers, and outdated conflict resolutions without overwriting either side.
- Added durable conflict recovery across `close`, `open`, and replacement utility processes, with
  active merge state reconstructed from repository refs and index stages.

### Changed

- Expanded the Node.js declarations and wrapper contract to expose up-to-date, fast-forward, and
  three-way merge outcomes with typed conflict details and materialization gates.
- Documented when applications must close and reopen long-lived SQLite handles around operations
  that may replace physical worktree files.

### Fixed

- Snapshot preparation now validates hydration markers and fetches the exact missing LSN range,
  repairing incomplete local state without re-fetching already available data.

### Compatibility

- Existing repository and Remote object formats are unchanged, and all pre-0.3.9 SDK methods retain
  their signatures. The merge surface is additive.
- Node.js 20 remains the minimum supported runtime; application-database integration tests using
  built-in `node:sqlite` run on Node.js 22 or newer.
