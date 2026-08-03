# Changelog

## Graft SDK 0.3.7 — 2026-08-04

### Added

- Added `recordPathMove`, an index-only SDK operation that records completed file or directory
  moves without re-reading artifact payloads or re-importing SQLite databases.
- Added first-class `renamed` changes with `previous_path` across status, staged and historical
  diffs, changed-path pages, bounded SQLite output, Node bindings, and TypeScript declarations.

### Changed

- SQLite path moves are paired by stable Volume identity, so moving an Eidos File and then editing
  it remains one rename with ordinary row changes instead of degrading into delete plus add.
- Exact artifact moves are paired by content object identity, directory moves expand tracked
  descendants atomically, and path-filtered history recognizes a rename from either its old or new
  path without hydrating unrelated SQLite contents.
- `diffSqlitePaths` can fall back to staged state and overlay staged move identity onto current
  worktree rows, keeping applications summary-first and free from rename reconstruction logic.

### Compatibility

- Existing commit objects and three-field path-change counts remain unchanged; renames contribute
  to the existing modified count while exposing their richer per-path representation.
- Existing SDK operations remain available. Recording a move is non-materializing and preserves
  the worktree as the application's authoritative local state.

## Graft SDK 0.3.6 — 2026-08-03

### Added

- Added checksummed, content-addressed SQLite page-ownership indexes that map changed overflow and
  freelist pages back to their logical tables without scanning unrelated million-row tables.
- Added regression coverage for overflow allocation and release, repeated checkpoints in one
  resident session, and repeated edits to the same SQLite pages.

### Changed

- Worktree probes now combine current `dbstat` ownership, baseline ownership, and validated
  freelist traversal to keep changed-table classification exact across page reuse.
- Page ownership remains replaceable derived cache data alongside page hashes and worktree probes;
  it can be deleted or rebuilt without changing snapshots, commits, or restore semantics.

### Fixed

- Changing a small `eidos__views` layout value no longer falls back to scanning unrelated large
  tables when SQLite allocates overflow pages from the freelist.
- Checkpointing an ordinary physical SQLite file no longer turns its staged snapshot into a live
  Volume binding, so edits made immediately after a checkpoint remain visible to status and the
  next checkpoint.

## Graft SDK 0.3.5 — 2026-08-02

### Added

- Added checksummed, content-addressed SQLite page indexes and worktree probes for large database
  files, with conservative fallback when fingerprints are racy or cache data is unavailable.
- Added stable physical SQLite readers, prepared stage state, and snapshot hydration proofs that
  stay outside the authoritative repository database write set.
- Added reproducible ordinary-file, Git-like, synthetic SQLite, and real Eidos benchmark suites
  with checked-in baseline/candidate raw results and a full performance report.

### Changed

- Checkpoint summaries now choose a changed-page-aware rowid algorithm or a bounded primary-key
  algorithm based on SQLite table layout instead of applying one scan strategy to every table.
- Staging retains the exact canonical SQLite snapshot and changed-table candidates for commit,
  avoiding a second read of the live application database.
- Status refresh preserves proven local classification across Remote projection changes and
  refreshes ahead/behind metadata independently.
- SQLite page indexes use the same content semantics as the Graft VFS across raw and online-backup
  snapshots, ignoring only SQLite's volatile page-1 counters and invalidating older cache schemas.

### Performance

- On the checked 460,689,408-byte Eidos fixture, dirty status fell from 5.40 seconds to 19.7 ms,
  selected metadata rows from 10.06 seconds to 1.69 ms, and a metadata-only checkpoint commit from
  21.30 seconds to 1.69 ms; peak RSS fell from 3.88 GiB to 672.7 MiB.
- A paired 5.6 MiB Git-like repository workload remained within benchmark noise for ordinary
  stage, commit, row diff, checkout, and filesystem Remote push, with unchanged storage bytes.

### Compatibility

- SQLite files smaller than 16 MiB bypass the persistent page index because an authoritative scan
  is cheaper at that scale. Derived caches are optional and may be deleted without affecting
  repository correctness.
- `stagePaths` remains API-compatible but still performs per-path core work; large explicit path
  batches remain a documented performance limit.

## Graft Remote 0.2.1 — 2026-08-02

### Added

- Added resumable multipart upload negotiation for large immutable Remote objects and Cloudflare
  R2 multipart storage support.
- Added retry-safe part upload, completion reconciliation, protocol tests, and fallback for Remotes
  that do not advertise multipart capabilities.

### Changed

- Large segment publication can resume completed parts instead of restarting the entire object
  after a timeout or interrupted request.

## Graft SDK 0.3.4 — 2026-08-01

### Added

- Added `diffSqlitePaths`, a bounded SQLite diff surface with a payload-free table summary mode
  and an independently paged row-detail mode for one explicitly selected table.
- Added opaque row cursors, cooperative cancellation, and aggregate/file telemetry for rows
  scanned, rows returned, truncation, and the active streaming or compatibility path.

### Changed

- Rowid tables now merge changes in stable rowid order one SQLite leaf page at a time and stop after
  one page plus a lookahead row instead of materializing every changed row.
- Empty-to-populated summary comparisons count leaf cells without decoding row payloads, keeping
  million-row history expansion responses small enough for Electron IPC and renderer consumption.
- Native-page-size Eidos `STRICT WITHOUT ROWID` tables now stream in primary-key order instead of
  copying both SQLite snapshots and materializing complete tables before applying the row limit.

### Performance

- On the checked 437,227,520-byte Eidos fixture, the all-table summary completed in about 1.8
  seconds with a 1,405-byte response; the first 100-row page for the million-row table completed in
  about 1.1 seconds with a 55,927-byte response after scanning 101 changed rows.

### Compatibility

- `diffPaths({ rows: true })` retains its existing full-row behavior. New large-diff consumers
  should use `diffSqlitePaths` summary-first paging.
- Custom-collation or descending `WITHOUT ROWID` primary keys and non-native SQLite page sizes
  return bounded payloads through the reported `materialized_compat` path, whose snapshot memory
  can still scale with database size.

## Graft SDK 0.3.3 — 2026-07-31

### Added

- Added an optional table filter to explicit-path SQLite row diffs, including safe telemetry that
  identifies the requested table and how many tables were scanned.
- Added logical row diffs for SQLite files introduced by an initial checkpoint or added/deleted
  between checkpoints.

### Changed

- Table-detail requests now scan only the selected SQLite table instead of every table in the
  database while preserving file and table summary metadata.
- Large Remote pushes now use phase-specific transfer timeouts, reconcile ambiguous publication
  outcomes, and expose stable SDK error codes for safe retry handling.

### Fixed

- Push retries no longer restart already completed immutable object uploads when the final
  publication response times out.
- Row-detail consumers can request one changed table without paying the cost of unrelated large
  tables in the same Eidos File.

## Graft SDK 0.3.0 — 2026-07-30

### Added

- Added persistent, generation-based incremental status with safe cache telemetry and crash-safe
  reuse across repository sessions and utility-process restarts.
- Added lightweight `repositoryMetadata` and credential-free `listRemotes` APIs that inspect no
  worktree paths.
- Added paged `historySummaries`, lazy `commitChangedPaths`, explicit-path `diffPaths`, bounded
  `isIgnoredPaths`/inventory queries, batch `stagePaths`/`restorePaths`, and index-only
  `untrackPaths`.
- Added cancellable `readPathContent({ revision, path, maxBytes })` for bounded, hash-validated
  artifact reads without exposing repository object storage.

### Changed

- Working and historical explicit-path diffs now hydrate only requested tree entries and blobs;
  history summary lists no longer load full trees or artifacts.
- Long-running status, diff, history, stage, restore, inventory, tree, and SQLite page loops now
  cooperate with `AbortSignal` without poisoning the retained session.
- SDK publication now uses npm OIDC Trusted Publishing and validates public installs on Node.js 20
  and 24 across all five advertised native targets.

### Fixed

- `commit` now advances from the staged canonical snapshot without replacing worktree SQLite files,
  preserving open application handles and inode identity.
- Concurrent file/directory, rename, unlink/recreate, and symlink changes now retry or return the
  structured `GRAFT_SDK_REPOSITORY_STALE` error instead of leaking raw filesystem errors.
- UTF-8 text classification now validates code points crossing the 8192-byte sniff boundary;
  incompatible persisted classifications are rejected through a snapshot schema bump.

### Performance

- On the checked-in 46,665-path macOS arm64 fixture, a validated persisted reopen is approximately
  291 ms, resident hot status approximately 101 ms, one-path diff approximately 19 ms, and 50-entry
  history summary approximately 10 ms.

### Compatibility

- Existing APIs remain available. `restore`, `restorePaths`, `pull`, and `cloneRepository` are the
  only operations that materialize worktree files; `commit`, stage, metadata, history, diff, and
  ignore APIs are non-materializing.
- Existing checkpoints are immutable. Corrected UTF-8 classification applies to newly staged
  snapshots and future diffs.

## Graft 0.11.0 — 2026-07-30

### Added

- HTTP Remotes can now negotiate `receive-pack` and `receive-bundle` so an
  incremental push publishes immutable objects and the ref update in two
  protocol requests instead of checking and uploading every object separately.
- `@eidos.space/graft-remote`, `@eidos.space/graft-remote-hono`, and
  `@eidos.space/graft-remote-cloudflare` provide reusable protocol, routing,
  and R2/Durable Object layers for compatible hosted Remotes.
- Push tracing reports safe phase timings, request counts, transferred bytes,
  correlation IDs, and `Server-Timing` data without exposing credentials,
  repository URLs, row contents, or local paths.

### Changed

- `@eidos.space/graft` 0.2.0 uses the bundled push protocols from resident
  repository sessions while preserving the public session API and fallback
  compatibility with existing HTTP Remotes.
- Snapshot and external-file objects are bundled deterministically, uploaded
  with bounded streaming, and published through the existing compare-and-swap
  ref boundary.

### Performance

- A one-row incremental SQLite push to the staging HTTP Remote now needs two
  data-plane requests rather than six, with measured resident-session median
  latency reduced from 5.23 seconds to 3.16 seconds.
- Reusing one HTTP connection for ref discovery and bundle publication removes
  duplicate DNS, TCP, TLS, and authentication setup within a push.

### Compatibility

- Clients automatically fall back to the public object-by-object protocol when
  a Remote does not advertise bundled receive capabilities.
- Non-fast-forward protection, force behavior, atomic ref publication, retry
  after partial upload, clone/fetch/pull, and logical SQLite row diffs retain
  their existing behavior.

## Graft 0.10.0 — 2026-07-29

### Added

- You can now reuse root and nested `.gitignore` files to keep generated or
  local-only worktree paths out of Graft with Git-compatible matching rules.

### Changed

- `.graftignore` now uses the same matching syntax as `.gitignore` and takes
  precedence when both files define rules in the same directory.
- You can stage changes to already tracked paths normally even when a later
  ignore rule matches them; ignored untracked paths still require
  `graft add --force`.

## Graft 0.9.0 — 2026-07-28

### Added

- Applications can now retain one in-process Graft repository session instead
  of starting a CLI process for every operation.
- The new `@eidos.space/graft` 0.1.0 package exposes the repository session as
  an ABI-stable Node-API 8 class for Node.js and Electron.
- SDK consumers can explicitly inject and rotate HTTP bearer credentials in
  memory without writing tokens to repository config, URLs, logs, or errors.
- The SDK covers repository initialization, status, add-all, commit, diff,
  history, restore, remote configuration, push, fetch, pull, and clone.

### Changed

- `RepositoryCommandService` is now a public, reusable control-plane boundary;
  the CLI keeps its existing one-command behavior while embedded sessions reuse
  the same runtime.
- Repository operations are serialized within one session, while different
  repositories can run concurrently.
- SDK release automation builds and tests native packages for macOS arm64/x64,
  Linux glibc arm64/x64, and Windows x64 before publishing the root npm package.

### Compatibility

- `restore`, `pull`, and clone materialize tracked worktree files. Applications
  must close affected SQLite handles before those calls and reopen them after
  completion.
- The SDK uses explicit in-memory credentials; the CLI retains its existing
  environment-variable credential compatibility path.
