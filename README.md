# Graft

**Version control for SQLite-backed apps and their files.**

Graft exists because more application state is becoming editable by AI. Once an
agent can create rows, rewrite documents, move attachments, update metadata, or
generate files, the application needs a way to inspect, accept, revert, branch,
merge, and sync those changes.

Git already solves that workflow for source code. It is less suited to the
runtime state of local-first apps, where the important data usually lives in
SQLite databases plus app-owned files such as attachments, imports, generated
assets, and metadata. Git can store those bytes, but it mostly sees SQLite
databases and many resource files as opaque artifacts rather than structured
application state.

Graft fills that gap. It gives SQLite-backed apps a Git-like repository for app
state: commit database and file changes together, diff SQLite revisions down to
rows, keep file artifacts with the database versions that reference them, expose
structured conflicts to an app UI, and push/pull through remotes.

## Why Graft

A typical SQLite-backed app has more state than one database file:

```text
app-data/
  data.sqlite
  search.sqlite
  settings.json
  attachments/
    note-42.png
    contract.pdf
```

The database may contain rows that reference those files. If a branch, rollback,
AI edit, or sync operation changes the database without changing the matching
files, the app can end up in an inconsistent state.

Graft's core idea is simple:

```text
A commit should record one coherent application state.
```

## What Graft Does

- **Versions SQLite databases and app files together**: track multiple database
  files, text files, binary files, external payloads, and repository refs as one
  app-state worktree.
- **Adds a Git-like workflow to app data**: initialize, stage, commit, branch,
  switch, diff, restore, fetch, pull, push, and merge.
- **Understands SQLite structure**: show row-level diffs, preserve compatible row
  changes, and auto-merge disjoint SQLite changes when it is safe.
- **Keeps file artifacts with the database state that references them**: small
  files can be stored inline, while large files use pointer objects and a local
  `.graft/store/files` payload cache.
- **Gives apps structured integration surfaces**: JSON command output and
  structured conflict artifacts let product UIs review, accept, reject, or
  resolve changes without scraping terminal text.

## Quickstart

Install the CLI:

```sh
curl -fsSL https://raw.githubusercontent.com/eidos-space/graft/main/install.sh | sh
```

Create a repository around an app worktree, then point SQL commands at the
database path you want to edit:

```sh
mkdir app-data
cd app-data

graft init
graft sql --db data.sqlite "CREATE TABLE notes(id TEXT PRIMARY KEY, body TEXT)"
graft add --all
graft commit -m "Initial version"
graft status --json
graft log --json
graft diff --rows --json HEAD
```

`graft init` creates only the repository metadata in `.graft/`; it does not
create a default SQLite database. SQLite-specific commands use an explicit
`--db` path, and committed SQLite snapshots are materialized back to that
repository-relative path.

Sync through a remote:

```sh
graft remote add origin s3://bucket/path
graft fetch
graft pull
graft push
```

Prebuilt CLI and SQLite extension archives are published on the
[GitHub releases page](https://github.com/eidos-space/graft/releases).

## Use From SQLite

The Graft SQLite extension lets applications call repository operations through
SQLite pragmas, which makes the workflow available from Electron, Node.js,
Python, Ruby, Swift, and any runtime with native SQLite support.

```sql
pragma graft_init;
pragma graft_add = '--all';
pragma graft_commit = 'Initial version';
pragma graft_json_status;
pragma graft_json_log;
pragma graft_json_diff = '--rows HEAD';
pragma graft_json_fetch;
pragma graft_json_pull;
pragma graft_json_push;
```

Conflict-oriented pragmas expose structured state for app UIs:

```sql
pragma graft_json_conflicts;
pragma graft_json_resolve_conflict = '--theirs assets/model.bin';
pragma graft_json_resolve_conflict = '--theirs --row docs 42';
pragma graft_merge_continue = 'Merge remote changes';
pragma graft_merge_abort;
```

## Learn More

- [CLI quickstart](./docs/src/content/docs/docs/get-started/cli.mdx)
- [SQLite extension guide](./docs/src/content/docs/docs/get-started/sqlite-extension.mdx)
- [App state versioning](./docs/src/content/docs/docs/concepts/app-state-versioning.mdx)
- [Repository model](./docs/src/content/docs/docs/concepts/repository-model.mdx)
- [CLI reference](./docs/src/content/docs/docs/reference/cli.mdx)
- [Pragmas reference](./docs/src/content/docs/docs/reference/pragmas.mdx)
- [Configuration reference](./docs/src/content/docs/docs/reference/configuration.mdx)

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

## Project Status

This repository is an experimental fork of
[orbitinghail/graft](https://github.com/orbitinghail/graft). The upstream
project is an open-source transactional storage engine for lazy, partial data
replication on top of object storage. This fork keeps that foundation and
applies it to app-state versioning for Eidos and other SQLite-backed apps.

Graft is not a general-purpose replacement for Git. It is focused on the app
state Git does not model well: SQLite databases, related app files, row-aware
diffs, structured conflicts, and object-storage-backed sync.

Upstream resources:

- [Original repository](https://github.com/orbitinghail/graft)
- [Documentation](https://graft.rs)
- [SyncConf 2025 talk](https://www.youtube.com/watch?v=QoKzDyH2MEA)
- [Blog post](https://sqlsync.dev/posts/stop-syncing-everything/)

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
