# Changelog

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
