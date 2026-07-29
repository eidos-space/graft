# Changelog

## Graft 0.10.0 — 2026-07-29

### Added

- Graft now reads `.gitignore` files from the repository root and nested
  directories using Git-compatible matching syntax and precedence.

### Changed

- `.graftignore` now uses the same matching syntax as `.gitignore` and takes
  precedence when both files define rules in the same directory.
- Ignored untracked paths still require `graft add --force`, while changes to
  already tracked paths can be staged normally.

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
