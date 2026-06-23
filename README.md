# Graft

This repository is a fork of
[orbitinghail/graft](https://github.com/orbitinghail/graft). The upstream
project is an open-source transactional storage engine for lazy, partial data
replication on top of object storage. This fork keeps that foundation and adds
application-facing SQLite repository workflows used by Eidos.

The fork should be treated as experimental. It currently focuses on making a
single SQLite database feel closer to a Git worktree: commit local changes, view
history, fetch and push to object storage, pull remote work, auto-merge compatible
database changes, and expose structured conflict artifacts to an application UI.

## What This Fork Adds

- **Git-like repository mode for SQLite**: repository pragmas expose operations
  such as `graft_init`, `graft_add`, `graft_commit`, `graft_status`,
  `graft_log`, `graft_show`, `graft_diff`, `graft_checkout`, `graft_reset`,
  `graft_fetch`, `graft_pull`, and `graft_push`.
- **JSON-first application API**: app integrations can use `graft_json_*`
  pragmas instead of parsing human-readable command output.
- **Physical SQLite worktree support**: repository state can track and
  materialize ordinary SQLite database files while still storing snapshot data
  through Graft.
- **Row-level SQLite diffs**: commit views and worktree diffs can describe table
  and row changes, not just page-level database file changes.
- **Row-level auto-merge**: disjoint row changes can be merged automatically even
  when the underlying SQLite file changed on both sides.
- **Conflict artifacts for UI flows**: unresolved row, schema, opaque, and file
  conflicts can be listed as structured JSON so a product can show focused
  conflict-resolution screens.
- **Per-row conflict resolution**: when multiple rows conflict in the same table,
  callers can choose ours/theirs per row rather than resolving the whole database
  file at once.
- **Preservation of non-conflicting changes**: compatible row changes are kept
  while true conflicts wait for manual resolution.
- **Snapshot resolution for pulled history**: historical commits fetched from
  remotes can be hydrated on demand for row-level diff and merge operations.
- **Hydrated snapshot hash normalization on push**: push can normalize stale
  hydrated snapshot commit hashes while still rejecting missing storage objects.

## Upstream Graft

Upstream Graft is designed for efficient data synchronization at the edge. Its
core ideas are still central to this fork:

- Lazy replication: clients fetch data on demand.
- Partial replication: clients only replicate the pages they need.
- Transactional object storage: object storage becomes a consistent storage
  layer for database snapshots.
- Strong consistency: readers can observe consistent snapshot views.
- Fast replicas: metadata and data are decoupled so replicas can start without a
  full replay of history.

Upstream resources:

- [Original repository](https://github.com/orbitinghail/graft)
- [Documentation](https://graft.rs)
- [SyncConf 2025 talk](https://www.youtube.com/watch?v=QoKzDyH2MEA)
- [Blog post](https://sqlsync.dev/posts/stop-syncing-everything/)

## SQLite Extension

The primary integration surface for this fork is the Graft SQLite extension. It
lets applications call repository operations through SQLite pragmas, which makes
the workflow available from Electron, Node.js, Python, Ruby, Swift, and any
runtime with native SQLite support.

Example repository workflow:

```sql
pragma graft_init;
pragma graft_add;
pragma graft_commit = 'Initial version';
pragma graft_json_status;
pragma graft_json_log;
pragma graft_json_diff = '--rows HEAD';
pragma graft_json_fetch;
pragma graft_json_pull;
pragma graft_json_push;
```

Conflict-oriented commands include:

```sql
pragma graft_json_conflicts;
pragma graft_json_resolve_conflict = '--theirs --row docs 42';
pragma graft_merge_continue = 'Merge remote changes';
pragma graft_merge_abort;
```

## Development

Common commands:

```sh
just test
cargo nextest run
just run sqlite test
cargo check
cargo fmt
cargo clippy
cargo build -p graft-ext --release
```

The SQLite integration tests live in `crates/graft-test/tests/sqlite.rs` and
cover the repository-mode workflows, row diffs, merge behavior, conflict
resolution, and remote push/pull flows.

## Status

This fork is not a general-purpose replacement for Git. It is a focused
experiment for SQLite-backed application data:

- It treats the database file as the worktree object.
- It uses Graft snapshots and object storage as the underlying storage layer.
- It exposes Git-like commands to applications.
- It adds row-aware behavior only where SQLite structure is available.
- It falls back to opaque/file-level conflict handling when row-level analysis is
  not safe.

## Contributing

For general coding style and development process, see [CONTRIBUTING.md](./CONTRIBUTING.md).
When working on this fork, keep application-specific policy out of the storage
engine where possible. Prefer structured, configurable primitives that Eidos or
another host application can interpret.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE))
- MIT license ([LICENSE-MIT](./LICENSE-MIT))

at your option.
