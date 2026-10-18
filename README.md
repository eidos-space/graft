# Graft

**Version history for SQLite databases and application files.**

[Try the Playground](https://graft.eidos.space/playground/) ·
[Documentation](https://graft.eidos.space/) ·
[Releases](https://github.com/eidos-space/graft/releases) ·
[简体中文](./README.zh-CN.md)

Graft records versions of your application's SQLite databases and related
files. You can inspect row changes, restore a recorded state, combine supported
branch edits, and send history to a remote. Your application keeps using
ordinary SQLite connections and file APIs.

## What gets recorded?

Suppose a note in `app.sqlite` refers to `attachments/report.pdf`, and
`settings.json` controls how the application displays it. You can record their
selected versions in one commit, then return to that recorded combination.

Graft understands supported SQLite content:

- Consistent snapshots, including committed WAL frames
- Content-addressed chunks that reuse unchanged SQLite content across versions
- Schema-, table-, and row-level diffs and merges
- Git-like commits, branches, tags, restore, and remote sync

```text
app-data/
├── app.sqlite
├── settings.json
└── attachments/
    └── report.pdf
```

A shared commit does not create a transaction across these files. Coordinate
application writes when the files must represent the same checkpoint.

Graft uses a Git-like workflow with its own repository and remote formats.

## Try Graft

The [Graft Playground](https://graft.eidos.space/playground/) runs the browser
CLI in an OPFS worktree. Try local commits, branches, row diffs, and conflicts
without installing anything. Native remote synchronization is unavailable in
the browser host.

Install the latest release on macOS or Linux:

```sh
curl -fsSL https://graft.eidos.space/install.sh | sh
```

Create a repository and save a SQLite database:

```sh
mkdir graft-demo
cd graft-demo

graft init
graft sql --db app.sqlite \
  "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT NOT NULL);"
graft sql --db app.sqlite \
  "INSERT INTO notes(body) VALUES ('first note');"
graft add app.sqlite
graft commit -m "Save initial state"
graft log
graft sql --db app.sqlite "SELECT id, body FROM notes;"
```

The query returns the first note. The log contains the recorded version.

Continue with the [CLI quickstart](https://graft.eidos.space/docs/quickstart/cli/).

Open an existing repository in the local browser viewer:

```sh
graft .
```

The React viewer keeps file contents and diffs beside an explorer with **All
files**, **Changes**, and **History**. It previews text changes and SQLite row
changes, browses current database tables, and can save small UTF-8 text files.
HTML and SVG use the source viewer. Worktree raster images, audio, video, and
PDF files have dedicated previews.
Saving changes the worktree; use the CLI to stage and commit. The server runs
on loopback until Ctrl+C. Use `graft ui . --no-open --port 4178` to choose a
port and print the URL without opening a browser.

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

## Saving, staging, and recording

| Action | What changes |
| --- | --- |
| SQLite transaction or file write | Current application files in the worktree |
| `graft add` | Selected snapshots and file bytes in the index (staging area) |
| `graft commit` | Local repository history, using staged content |
| `graft push` | Remote history, using recorded commits |
| Restore or switch | Worktree files, using recorded content |

If you insert a note after `add`, `commit` still records the earlier snapshot.
The new note remains an unstaged worktree change. Push does not upload it as a
new version until you stage and commit it.

A repository stores its history in `.graft/` beside the normal worktree.
Applications keep using their existing SQLite library and file APIs.

Staging captures committed WAL content without a manual checkpoint. Restore
and branch switching can replace database files; close affected application
connections before those operations.

The [architecture guide](https://graft.eidos.space/docs/graft-book/) explains
these boundaries. Its [lab](https://graft.eidos.space/docs/graft-book/hands-on-lab/)
lets you predict a result, run the command, and check it with a SQL query.

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
