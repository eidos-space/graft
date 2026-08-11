> [!NOTE]
> Graft began as a fork of
> [orbitinghail/graft](https://github.com/orbitinghail/graft). Its original
> transactional storage engine now powers this project's SQLite storage layer.
> This repository is independently maintained and is no longer a GitHub fork.

**English** · [简体中文](./README.zh-CN.md)

# Graft

**Version control for SQLite-backed application state.**

[Try the Playground](https://graft.eidos.space/playground/) ·
[Documentation](https://graft.eidos.space/) ·
[Releases](https://github.com/eidos-space/graft/releases) ·
[CLI reference](https://graft.eidos.space/docs/reference/cli/)

Graft gives SQLite databases and app-owned files one coherent history. It adds
Git-like commits, branches, row-level diffs, merges, restore, and remote sync.
The default workflow uses ordinary SQLite files and libraries—no custom VFS is
required.

## Why Graft?

Application state rarely lives in one table or one file:

```text
app-data/
  data.sqlite
  settings.json
  attachments/
    avatar.png
    invoice.pdf
```

SQLite keeps each database transaction consistent, but it does not give the
whole directory version history. Git can track the directory, but it usually
treats a SQLite database as an opaque binary file.

Graft records the database and its related files together:

| Need | What Graft provides |
| --- | --- |
| Review database changes | Table- and row-level SQLite diffs |
| Store database history incrementally | Reuse unchanged 4 KiB chunks and append only changed data instead of copying the entire database |
| Save a coherent app state | One commit for databases and their related files |
| Experiment safely | Branches, tags, restore, and automatic merges for compatible changes |
| Handle conflicts in an app | Structured conflicts and JSON output |
| Move history between devices | Filesystem, S3-compatible, and HTTP remotes |
| Embed repository workflows | A Node.js/Electron SDK with long-lived sessions |

Graft is a good fit for local-first apps, AI-assisted editing, user-visible
history, checkpoints, change review, and applications whose database rows refer
to files outside SQLite.

Graft is not a replacement for SQLite transactions, application authorization,
real-time query replication, or Git source control.

## Try Graft

### In your browser

The [Graft Playground](https://graft.eidos.space/playground/) runs the real CLI
in WebAssembly and stores everything in the browser. It is the fastest way to
explore commits, branches, SQLite row diffs, and conflicts without installing
anything.

### On your machine

Install Graft v0.13.0:

```sh
curl -fsSL https://graft.eidos.space/install.sh \
  | GRAFT_VERSION=0.13.0 sh

graft --version
```

Omit `GRAFT_VERSION=0.13.0` to install the latest published release. Prebuilt
archives are available on the
[releases page](https://github.com/eidos-space/graft/releases).

Create a repository containing a SQLite database and an app-owned file:

```sh
mkdir graft-demo
cd graft-demo

graft init
graft sql --db data.sqlite \
  "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT NOT NULL);"
graft sql --db data.sqlite \
  "INSERT INTO notes(body) VALUES ('first note');"

mkdir attachments
printf 'hello\n' > attachments/readme.txt

graft status
graft add --all
graft commit -m "Save initial app state"
```

Make another database change and inspect it as rows instead of binary bytes:

```sh
graft sql --db data.sqlite \
  "INSERT INTO notes(body) VALUES ('second note');"

graft add data.sqlite
graft commit -m "Add second note"

graft diff --rows HEAD~1 HEAD data.sqlite
graft log
```

You now have two restorable versions of the database and its related files.
Continue with the
[CLI quickstart](https://graft.eidos.space/docs/quickstart/cli/) to try
branches, merges, and restore.

## How It Works

A Graft repository lives beside your application's normal worktree:

```text
app-data/
  data.sqlite          normal SQLite file
  settings.json        normal app file
  attachments/
  .graft/              history, index, refs, objects, and payload cache
```

The basic workflow is familiar:

1. Your application writes to SQLite and its files as usual.
2. `graft add` captures a consistent committed SQLite snapshot and stages
   files.
3. `graft commit` records those paths as one application state.
4. Diff, branch, merge, restore, and sync operate on that history.

For physical SQLite files, staging includes committed WAL frames without
requiring a manual checkpoint. Graft compares the captured image in 4 KiB
storage chunks and reuses unchanged data.

Commands that change the checked-out state materialize database snapshots and
files back into the worktree. Close long-lived SQLite connections before
`switch`, `checkout`, `restore`, `pull`, merge completion, or a hard
reset. Graft refuses to replace a database while another writer holds it.

## Use Graft With an Existing SQLite App

The CLI works with the same physical database file as `sqlite3` or your
application's SQLite library:

```sh
sqlite3 data.sqlite \
  "INSERT INTO notes(body) VALUES ('written by sqlite3');"

graft add data.sqlite
graft commit -m "Import SQLite transaction"
```

No custom VFS is required for this workflow. Add other app-owned paths with
`graft add <path>` or stage the whole worktree with `graft add --all`.

## Choose an Integration

Start with the CLI unless your application needs a more specialized boundary.

| Integration | Use it when |
| --- | --- |
| [CLI](https://graft.eidos.space/docs/quickstart/cli/) | You are evaluating Graft, scripting workflows, or running one-shot commands. |
| CLI with [JSON output](https://graft.eidos.space/docs/reference/json-output/) | Your application or agent needs structured status, diff, history, conflict, or sync results. |
| [Node.js/Electron SDK](https://graft.eidos.space/docs/guides/node-electron-sdk/) | A Node.js or Electron app needs a long-lived in-process repository session. |
| [Remote service packages](https://graft.eidos.space/docs/guides/http-remote/) | You want to host the Graft HTTP remote protocol. |

Install the resident Node.js/Electron SDK with:

```sh
pnpm add @eidos.space/graft
```

## Common Workflows

| Workflow | Commands |
| --- | --- |
| Inspect state | `status`, `log`, `show`, `diff --rows` |
| Record state | `add`, `rm`, `commit` |
| Move through history | `checkout`, `restore`, `export`, `reset` |
| Work with branches | `branch`, `switch`, `merge`, `conflicts`, `resolve` |
| Synchronize | `remote`, `ls-remote`, `fetch`, `pull`, `push` |
| Maintain storage | `audit`, `gc`, `payload` |

See the [CLI reference](https://graft.eidos.space/docs/reference/cli/) for
arguments, JSON schemas, and examples.

## Remotes

Graft supports local filesystem, S3, S3-compatible object storage, and Graft
HTTP remotes:

```text
fs:///absolute/path
s3://bucket/prefix
s3_compatible://bucket/prefix?endpoint=https://...
https://host/namespace/repository
graft+http://127.0.0.1:8787/namespace/repository
```

```sh
export GRAFT_REMOTE_TOKEN='grt_...'

graft remote add origin https://example.com/acme/archive
graft push origin main
graft pull origin main
```

Bearer tokens come from the environment and are not stored in remote URLs. See
[Sync with remotes](https://graft.eidos.space/docs/guides/sync-remotes/) and
the [HTTP remote guide](https://graft.eidos.space/docs/guides/http-remote/).

## Project Status

Graft is experimental. The CLI, repository configuration, JSON output,
Node.js/Electron SDK, and remote service protocol are the intended integration
surfaces. Storage layouts, object serialization, and internal
Rust module boundaries are implementation details and may change.

## Documentation

- [What is Graft?](https://graft.eidos.space/docs/overview/what-is-graft/)
- [CLI quickstart](https://graft.eidos.space/docs/quickstart/cli/)
- [Repository model](https://graft.eidos.space/docs/concepts/repository-model/)
- [SQLite snapshots](https://graft.eidos.space/docs/concepts/sqlite-snapshots/)
- [Track databases and files](https://graft.eidos.space/docs/guides/track-databases-and-files/)
- [Diff rows and files](https://graft.eidos.space/docs/guides/diff-rows-and-files/)
- [Merge conflicts](https://graft.eidos.space/docs/guides/merge-conflicts/)
- [Node.js and Electron SDK](https://graft.eidos.space/docs/guides/node-electron-sdk/)
- [CLI reference](https://graft.eidos.space/docs/reference/cli/)
- [Performance benchmarks](./crates/graft-bench/README.md)

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for the development workflow and
coding guidelines.

```sh
just test
cargo check
cargo fmt
cargo clippy
```

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE))
- MIT license ([LICENSE-MIT](./LICENSE-MIT))

at your option.
