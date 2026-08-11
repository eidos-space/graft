# Graft SDK changelog

This changelog covers the independently versioned `@eidos.space/graft` package. Core Graft CLI and
SQLite extension releases are documented in the repository-level `CHANGELOG.md`.

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
