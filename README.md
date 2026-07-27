# Graft

**Version control for SQLite-backed applications and their files.**

[Documentation](https://graft.eidos.space/) ·
[Releases](https://github.com/eidos-space/graft/releases) ·
[CLI reference](https://graft.eidos.space/docs/reference/cli/)

Graft records SQLite databases and app-owned files as coherent application
states. It provides Git-like history, branches, diffs, merges, restore, and
remote sync while keeping SQLite databases usable as ordinary files.

```text
SQLite owns transactions.
Graft owns history.
```

## Repository Model

A Graft repository lives alongside the application worktree:

```text
app-data/
  data.sqlite
  search.sqlite
  settings.json
  attachments/
    note-42.png
    contract.pdf
  .graft/
```

Applications read and write the files under `app-data/`. Graft stores repository
metadata, refs, staged state, typed objects, SQLite page history, and external
payload caches under `.graft/`.

Each commit contains one typed tree:

```text
data.sqlite                 SQLite snapshot
settings.json               inline file
attachments/contract.pdf    external file payload
```

This lets a branch, restore, merge, or remote sync move the database and the
files referenced by its rows as one unit.

## Capabilities

- Track multiple SQLite databases, text files, binary files, and external
  payloads in one repository.
- Stage and commit application state with `status`, `add`, `rm`, `commit`,
  `log`, `show`, `checkout`, `restore`, `export`, and `reset`.
- Compare SQLite snapshots by table and row with `diff --rows`.
- Create branches and tags, switch worktrees, and merge compatible row changes.
- Represent unresolved file, schema, and row conflicts as structured repository
  state.
- Sync through filesystem, S3-compatible, and Graft HTTP remotes.
- Expose JSON results for application UIs, automation, and agent workflows.
- Verify and maintain repositories with `audit`, `gc`, and `payload` commands.

## Quickstart

Install Graft v0.8.0:

```sh
curl -fsSL https://raw.githubusercontent.com/eidos-space/graft/main/install.sh \
  | GRAFT_VERSION=0.8.0 sh

graft --version
```

Create a repository and commit a SQLite database together with an app file:

```sh
mkdir app-data
cd app-data

graft init
graft sql --db data.sqlite \
  "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT NOT NULL);"
graft sql --db data.sqlite \
  "INSERT INTO notes(body) VALUES ('first note');"

mkdir attachments
printf 'hello\n' > attachments/readme.txt

graft status
graft add --all
graft commit -m "Seed app state"
```

Create another version and inspect its row-level changes:

```sh
graft sql --db data.sqlite \
  "INSERT INTO notes(body) VALUES ('second note');"
graft add data.sqlite
graft commit -m "Add second note"

graft diff --rows HEAD~1 HEAD data.sqlite
graft log --json
```

`graft init` creates `.graft/`. SQLite-specific commands take an explicit
`--db` path; repository-wide commands discover `.graft/` from the current
directory.

## SQLite Snapshots

`graft sql --db data.sqlite` and standard SQLite libraries open the same
physical worktree file. After an application transaction commits, stage the
database path:

```sh
sqlite3 data.sqlite \
  "INSERT INTO notes(body) VALUES ('written by sqlite3');"
graft add data.sqlite
graft commit -m "Import SQLite transaction"
```

Staging a SQLite database:

1. Uses SQLite's online backup API to capture a consistent committed state.
2. Includes committed WAL frames without requiring a manual checkpoint.
3. Excludes uncommitted transactions.
4. Compares 4 KiB pages with the staged snapshot or `HEAD`.
5. Appends only changed pages to Graft storage.

The same workflow supports WAL and rollback-journal databases. Commands that
change the checked-out state materialize snapshots back to physical SQLite
files. Close long-lived database connections before `switch`, `checkout`,
`restore`, `pull`, merge completion, or hard reset; Graft refuses to replace a
database while another writer holds it.

## Branches, Diffs, And Merges

Branches cover every tracked database and file in the worktree:

```sh
graft switch -c feature/search

graft sql --db data.sqlite \
  "INSERT INTO notes(body) VALUES ('branch note');"
graft add --all
graft commit -m "Add search data"

graft switch main
graft --db data.sqlite merge feature/search
```

When supported tables contain disjoint row edits, Graft can merge them and
stage the result. Row identity comes from SQLite `rowid` for ordinary tables
and the declared primary key for `WITHOUT ROWID` tables, including composite
keys and `STRICT` tables. Same-row edits, schema changes, opaque SQLite
surfaces, and conflicting file changes remain explicit conflicts for the
application or user to resolve.

If a merge stops with conflicts:

```sh
graft --db data.sqlite conflicts --json
graft --db data.sqlite resolve --ours data.sqlite
graft merge --continue -m "Merge feature/search"
```

## Files And External Payloads

Small files are stored in repository objects. Large files and configured paths
use content-addressed payloads under `.graft/store/files` and are transferred
with repository sync.

```sh
graft config set files.external_paths "assets/**, attachments/**"
graft add --all

graft payload status --json HEAD
graft payload fetch --remote origin HEAD
graft payload prune
```

Database rows and the file payloads they reference therefore share the same
commit history without requiring large binary content inside SQLite.

## Remotes

Graft supports these remote URI families:

```text
memory
fs:///absolute/path
s3://bucket/prefix
s3_compatible://bucket/prefix?endpoint=https://...
https://host/namespace/repository
graft+http://127.0.0.1:8787/namespace/repository
```

Configure and use an HTTPS remote with a bearer token:

```sh
export GRAFT_REMOTE_TOKEN='grt_...'

graft remote add origin https://example.com/acme/archive
graft ls-remote origin
graft push origin main
graft fetch origin main
graft pull origin main
```

Tokens come from the environment and are not stored in the remote URL. The
[Graft Remote Service Protocol](https://graft.eidos.space/docs/reference/remote-protocol/)
defines the versioned HTTP contract, atomic ref operations, immutable objects,
range reads, listing, and error behavior.

The repository includes reusable TypeScript components for remote services:

- [`@eidos.space/graft-remote`](./packages/graft-remote): framework-neutral
  protocol engine and types.
- [`@eidos.space/graft-remote-hono`](./packages/graft-remote-hono): Hono routing
  adapter.
- [`@eidos.space/graft-remote-cloudflare`](./packages/graft-remote-cloudflare):
  Cloudflare authentication and storage adapters.
- [`services/graft-remote-cloudflare`](./services/graft-remote-cloudflare):
  deployable reference service using Durable Objects and R2.

## Application Integration

The CLI is Graft's repository control plane. Commands support structured JSON
for desktop applications, web services, scripts, and agents:

```sh
graft status --json
graft diff --json --rows HEAD~1 HEAD data.sqlite
graft log --json --limit 50
graft --db data.sqlite conflicts --json
graft push --json origin main
```

The SQLite extension provides `vfs=graft` for applications that store live
SQLite pages in a Graft Volume. It also exposes `graft_version` and
`graft_debug_*` diagnostics for that volume:

```sql
.load ./libgraft_ext
.open "file:/absolute/path/to/app-data/data.sqlite?vfs=graft"
PRAGMA graft_version;
```

Repository status, staging, commits, branches, merges, and remotes are handled
through the CLI. A logical database should use either its physical worktree
file or the Graft VFS as its write path.

## Documentation

- [What is Graft?](https://graft.eidos.space/docs/overview/what-is-graft/)
- [CLI quickstart](https://graft.eidos.space/docs/quickstart/cli/)
- [Repository model](https://graft.eidos.space/docs/concepts/repository-model/)
- [SQLite snapshots](https://graft.eidos.space/docs/concepts/sqlite-snapshots/)
- [Track databases and files](https://graft.eidos.space/docs/guides/track-databases-and-files/)
- [Connect an HTTP remote](https://graft.eidos.space/docs/guides/http-remote/)
- [CLI reference](https://graft.eidos.space/docs/reference/cli/)
- [Remote service protocol](https://graft.eidos.space/docs/reference/remote-protocol/)
- [VFS PRAGMAs](https://graft.eidos.space/docs/reference/pragmas/)

## Development

```sh
just test
cargo nextest run
just run sqlite test
cargo check
cargo fmt
cargo clippy
cargo build -p graft-ext --release

pnpm check:remote
pnpm test:remote

cd docs
pnpm build
```

See [CONTRIBUTING.md](./CONTRIBUTING.md) for coding style and development
workflow.

## Project Status

Graft is experimental. The CLI, repository configuration, JSON output, and
remote service protocol are its intended integration surfaces. Storage LSNs,
page and segment layouts, debug PRAGMAs, object serialization, and internal
module boundaries are implementation details.

Graft is built on the transactional storage engine from
[`orbitinghail/graft`](https://github.com/orbitinghail/graft).

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE))
- MIT license ([LICENSE-MIT](./LICENSE-MIT))

at your option.
