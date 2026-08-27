# Graft

**Git for SQLite.**

[Try the Playground](https://graft.eidos.space/playground/) ·
[Documentation](https://graft.eidos.space/) ·
[Releases](https://github.com/eidos-space/graft/releases) ·
[简体中文](./README.zh-CN.md)

Git tracks a SQLite database as an opaque binary file. Graft adds
storage-efficient snapshots, schema-aware merges, and row-level diffs while
keeping the worktree as ordinary SQLite files.

## Why Graft?

A custom diff driver can render a readable view of `data.sqlite` for review.
Git's storage and merge still operate at the file level, leaving SQLite
storage, schemas, and rows invisible.

Graft adds those database semantics:

- Consistent snapshots, including committed WAL frames
- Content-addressed chunks that reuse unchanged SQLite content across versions
- Schema-, table-, and row-level diffs and merges
- Git-like commits, branches, tags, restore, and remote sync

Related files can be recorded in the same commit as the database:

```text
app-data/
  data.sqlite
  settings.json
  attachments/
```

Graft uses a familiar Git-like workflow with its own repository and remote
formats.

## Try Graft

The [Graft Playground](https://graft.eidos.space/playground/) runs the real CLI
in your browser. It is the fastest way to explore commits, branches, row-level
diffs, and conflicts without installing anything.

Install the latest release on macOS or Linux:

```sh
curl -fsSL https://graft.eidos.space/install.sh | sh
```

Create a repository and save a SQLite database:

```sh
mkdir graft-demo
cd graft-demo

graft init
graft sql --db data.sqlite \
  "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT NOT NULL);"
graft sql --db data.sqlite \
  "INSERT INTO notes(body) VALUES ('first note');"
graft add --all
graft commit -m "Save initial state"
graft log
```

Continue with the [CLI quickstart](https://graft.eidos.space/docs/quickstart/cli/).

## Choose an Integration

| Integration | Use it for |
| --- | --- |
| [CLI](https://graft.eidos.space/docs/quickstart/cli/) | Evaluation, scripts, agents, and one-shot commands |
| [Structured JSON output](https://graft.eidos.space/docs/reference/json-output/) | Stable application and automation boundaries |
| [Node.js SDK](https://graft.eidos.space/docs/sdk/) | Long-lived, in-process repository sessions |
| [Remote service packages](https://graft.eidos.space/docs/remotes/) | Hosting the Graft HTTP remote protocol |

Install the Node.js SDK with:

```sh
pnpm add @eidos.space/graft
```

## Who Uses Graft?

### Eidos Lite

[Eidos Lite](https://eidos.space/download#eidos-lite) is a local-first desktop
app for `.eidos` relational spreadsheets and ordinary files. It uses Graft to:

- Record an entire local workspace as one version
- Show file changes and row-level SQLite changes before saving
- Inspect and restore earlier versions without rewriting history
- Synchronize workspace history through optional Eidos Sync

The integration is open source in the
[Eidos repository](https://github.com/mayneyao/eidos/tree/dev/apps/eidos-lite-desktop).

Using Graft in a product? Open a pull request to add it here.

## Core Model

1. Your application writes ordinary SQLite databases and files.
2. `graft add` captures a consistent snapshot and stages related paths.
3. `graft commit` records them as one application state.
4. Diff, branch, merge, restore, and sync operate on that history.

A repository stores its history in `.graft/` beside the normal worktree.
Applications keep using their existing SQLite library and file APIs.

## Project Status

Graft is experimental. The CLI, structured output, Node.js SDK, and
remote protocol are supported integration surfaces. Storage layouts and
internal Rust modules remain implementation details.

Graft complements SQLite transactions; it does not replace application
authorization, real-time replication, or Git source control.

## Development

See [CONTRIBUTING.md](./CONTRIBUTING.md) for the full workflow.

```sh
cargo check --workspace --all-targets
just test

pnpm check:remote
pnpm test:remote
```

The implementation-aligned specifications live in
[`docs/specs`](./docs/specs).

## Lineage

Graft began as a fork of
[orbitinghail/graft](https://github.com/orbitinghail/graft). Its original
transactional storage engine now powers this project's SQLite storage layer.
This repository is independently maintained and is no longer a GitHub fork.

## License

Licensed under either the [Apache License 2.0](./LICENSE-APACHE) or the
[MIT License](./LICENSE-MIT), at your option.
