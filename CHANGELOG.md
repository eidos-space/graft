# Changelog

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
