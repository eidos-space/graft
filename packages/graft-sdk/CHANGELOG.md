# Graft SDK changelog

This changelog covers the independently versioned `@eidos.space/graft` package. Core Graft CLI and
SQLite extension releases are documented in the repository-level `CHANGELOG.md`.

## Unreleased

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
