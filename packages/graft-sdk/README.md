# Graft SDK for Node.js and Electron

`@eidos.space/graft` embeds Graft as a long-lived, in-process repository SDK for Node.js and
Electron. It is the native application integration surface used by Eidos Lite Desktop. The Rust
session and Node-API crates remain repository-internal implementation crates and are not published
to crates.io.

The useful analogy is:

| Git ecosystem | Graft SDK |
| --- | --- |
| libgit2: an in-process repository handle | `graft-sdk`: the Rust repository session core |
| NodeGit: a native JavaScript repository class | `@eidos.space/graft`: an ABI-stable Node-API class |

This is a design analogy only. Graft does not depend on libgit2 or NodeGit, does not implement the
Git protocol, and does not copy a repository protocol. The session delegates every operation to
Graft's existing `RepositoryCommandService`, repository implementation, and official HTTP remote.
There is no daemon, JSON-stdin transport, or Graft CLI subprocess in the SDK call path.

## Architecture

```text
Electron utility process
  └─ JavaScript RepositorySession
       └─ Node-API 8 native class (one Arc-owned handle)
            └─ Rust RepositorySession (one mutex per repository session)
                 └─ retained RepositoryCommandService
                      ├─ Graft Repository / RepoRuntimeRegistry
                      └─ official FS, S3, or HTTP Remote
```

Each native method returns a promise and runs its blocking Rust work on a libuv worker thread.
Calls on one `RepositorySession` serialize for the full operation. Sessions for different
repositories have independent locks and can run concurrently.

## Install

```sh
pnpm add @eidos.space/graft
```

The root package selects one optional native package for the current host. The release workflow
publishes and tests Node-API 8 binaries for macOS arm64/x64, Linux glibc arm64/x64, and Windows
x64. Linux musl and other platform/CPU combinations fail explicitly instead of compiling during
install or downloading an unverified binary.

## Build and test from source

Prerequisites are the repository Rust toolchain, Node.js 20 or newer, and pnpm. The release matrix
runs the session contract on Node.js 20 and 24; the two application-database integration cases
require Node.js 22 or newer for its built-in `node:sqlite`.

```sh
pnpm --dir packages/graft-sdk build:native
pnpm --dir packages/graft-sdk test
```

The build produces:

```text
packages/graft-sdk/native/graft-sdk.darwin-arm64.node
```

The local binary is intentionally ignored by Git. It is a Mach-O arm64 dynamic library that exports
Node-API 8, so it is insulated from Node's V8 ABI and `NODE_MODULE_VERSION`. It is still
platform/architecture-specific. An Electron package must copy it outside ASAR, or list `*.node`
under `asarUnpack`, and load it in the utility process. `GRAFT_SDK_NATIVE_PATH` may point tests or
packaging harnesses at an explicit addon path.

Published packages use the standard native optional-dependency model: the root package contains
the JavaScript wrapper, declarations, README, and changelog; each platform package contains exactly
one `.node` binary plus `os`, `cpu`, and (on Linux) `libc` constraints. Consumers do not need a
Rust toolchain or system SQLite library, and the package has no install script.

## Use

```js
const { RepositorySession } = require("@eidos.space/graft")

const session = await RepositorySession.open(spaceRoot)
try {
  const status = await session.statusIncremental()
  const diff = await session.diffPaths({
    paths: status.status.paths.map(({ path }) => path),
    rows: true,
    // Optional: scan just the table the user opened.
    table: "customers",
    limit: 100,
  })

  // For large SQLite changes, first load table counts with no row payloads.
  const summary = await session.diffSqlitePaths({
    paths: ["space.eidos"],
    mode: "summary",
    limit: 1,
  })
  const firstRows = await session.diffSqlitePaths({
    paths: ["space.eidos"],
    mode: "rows",
    table: "customers",
    rowLimit: 100,
    limit: 1,
  })
  const nextRows = firstRows.paths[0].diff.files[0].next_cursor
    ? await session.diffSqlitePaths({
        paths: ["space.eidos"],
        mode: "rows",
        table: "customers",
        rowLimit: 100,
        rowAfter: firstRows.paths[0].diff.files[0].next_cursor,
        limit: 1,
      })
    : undefined
} finally {
  await session.close()
}
```

Eidos Lite should keep one session per open Space in its utility process rather than constructing a
session per command:

```js
const sessions = new Map()

async function openSpace(spaceId, root) {
  const session = await RepositorySession.open(root)
  sessions.set(spaceId, session)
  return session
}

async function closeSpace(spaceId) {
  const session = sessions.get(spaceId)
  sessions.delete(spaceId)
  await session?.close()
}
```

The Electron main/renderer boundary can continue to use typed IPC, but the utility process must
load this package directly. It must not launch `graft`, host a long-running CLI daemon, or proxy
commands over JSON stdin.

## API and worktree materialization

“Materializes worktree” means that an operation can replace, create, or remove physical tracked
files in the Space. Changes confined to `.graft` are not counted.

| Method | Purpose | Materializes tracked worktree files |
| --- | --- | --- |
| `open`, `close`, `reopen` | Manage the retained repository runtime | No |
| `init` | Initialize `.graft` metadata | No |
| `status` | Inspect worktree and index state | No |
| `statusIncremental` | Inspect status with a stable generation/change token and safe cache telemetry | No |
| `repositoryMetadata` | Read head, branch, upstream, and repository format without scanning the worktree | No |
| `listRemotes` | Read a credential-free remote URL/config projection without scanning the worktree | No |
| `addAll` | Read/import the current worktree into the index | No |
| `stagePaths` | Stage up to 1,000 explicit paths in one serialized SDK call | No |
| `recordPathMove` | Preserve tracked identity after a physical file or directory rename | No |
| `untrackPaths` | Remove up to 1,000 explicit files from the index without touching the worktree | No |
| `commit` | Advance repository history | No |
| `diff` | Compare worktree, index, or revisions | No |
| `diffPaths` | Diff a page of explicit changed file paths (up to 100) | No |
| `diffSqlitePaths` | Read SQLite table summaries or one bounded row page for explicit paths | No |
| `readPathContent` | Read bounded artifact content at one immutable revision | No |
| `history` | Read commit history and status | No |
| `historySummaries` | Read up to 500 lightweight commit summaries without trees/blobs | No |
| `commitDetails` | Lazily hydrate one full commit | No |
| `commitChangedPaths` | Lazily page one commit's first-parent changed paths (up to 100) | No |
| `isIgnoredPath` | Apply nested `.gitignore` / `.graftignore` semantics | No |
| `isIgnoredPaths` | Apply shared ignore/index caches to up to 1,000 file or directory paths | No |
| `inventory` | Page tracked, untracked, ignored, or tracked-and-ignored paths | No |
| `restore` | Replace selected paths from a revision | **Yes** |
| `restorePaths` | Restore up to 1,000 explicit paths in one serialized SDK call | **Yes** |
| `configureRemote` | Persist remote URL/upstream metadata | No |
| `push` | Send objects/refs to a remote | No |
| `fetch` | Receive objects/refs into `.graft` | No |
| `pull` | Fetch, integrate, and check out the result | **Yes** |
| `cloneRepository` | Populate a new worktree from a remote | **Yes** |
| `planMerge` | Compute an immutable up-to-date, fast-forward, or three-way plan | No |
| `getMergePolicy`, `validateMergePolicy`, `setMergePolicy` | Read, validate, or CAS-update the versioned finite merge policy | No |
| `applyMerge` | Apply a reviewed plan under HEAD/plan-token guards | **Yes** |
| `getMergeStatus` | Reconstruct the active merge from `ORIG_HEAD`, `MERGE_HEAD`, and index stages | No |
| `listMergePaths`, `listMergeConflicts` | Page merge paths and selected-path conflict details | No |
| `readMergeVersion` | Read bounded base/ours/theirs/result content | No |
| `diffMergeSqlite` | Diff immutable base/ours/theirs SQLite versions during an active merge | No |
| `setMergePathResult`, `resolveMergeRow`, `resolveMergeCell`, `resolveMergeTable` | Select ours/theirs for a path, SQLite row, cell, or table | **Yes** |
| `unresolveMergePath` | Restore a resolved path to its original merge conflict stages | **Yes** |
| `stageMergeSqliteResult` | Validate and stage an application-edited SQLite candidate without replacing the worktree | No |
| `prepareSemanticMerge`, `recordSemanticMergeConflicts` | Prepare/reopen a durable private provider workspace and persist opaque domain conflicts | No |
| `acceptSemanticMergeResult` | Validate, materialize, and stage a provider-owned SQLite result | **Yes** |
| `writeAndStageTextResult` | Atomically replace and stage an edited UTF-8 result | **Yes** |
| `continueMerge`, `abortMerge` | Commit or restore a guarded active merge | **Yes** |

`operationMaterializesWorktree(name)` exposes this contract to the Eidos gate. Before any method
marked **Yes**, Eidos must checkpoint and close application SQLite handles for paths that can be
replaced. `stagePaths` and merge inspection methods never materialize the worktree. Reopen
application handles after the SDK promise settles; the Graft repository session itself stays open.
Merge apply and mutation results also return `worktree_paths`, a sorted, deduplicated list of the
repository-relative paths actually created, replaced, or removed by that completed operation. Use
the conservative gate before the call and this exact list to bound validation and refresh work
afterward. In particular, an exact-token `continueMerge` may return `worktree_paths: []` when the
validated final SQLite candidate was already installed by the resolving operation; the conservative
before-call gate remains **Yes** because callers cannot assume that fast path in advance.

`addAll` reads SQLite files and their committed/WAL state but does not replace them. Eidos should
still checkpoint its application databases before snapshotting when it needs a deterministic
commit boundary. `commit` advances history from the staged canonical snapshot without writing that
snapshot back to the worktree, so an open application SQLite handle keeps the same file identity.

After the host application physically renames a tracked file or directory, call
`recordPathMove({ previousPath, path })`. Graft atomically moves the existing index identities,
expands tracked directory descendants internally, and does not read or re-import payloads. Status,
staged diff, commit changed paths, and filtered historical diff then expose one `renamed` change
with `previous_path`. Exact object moves discovered without this hint are also paired during diff,
but the explicit operation is required to preserve a moved SQLite snapshot identity before the
next stage.

## Lifecycle, writers, and recovery

A constructed session starts `closed`. `open()` creates and retains the Graft runtime; calling it
again while open returns `GRAFT_SDK_SESSION_ALREADY_OPEN`. `close()` changes the lifecycle to
`closing` before waiting for the current operation. New or queued repository work then fails with
`GRAFT_SDK_SESSION_CLOSING`, the in-flight operation is allowed to finish, and the runtime lock is
released. `close()` is idempotent. `reopen()` drops and reconstructs the runtime from durable state.

Only one retained runtime may own a repository's Graft storage lock. A second session or an
external Graft writer gets `GRAFT_SDK_REPOSITORY_BUSY`. Different repository paths can run in
parallel. External changes to ordinary worktree files are observed by subsequent `status`,
`diff`, and `addAll` calls.

If an Electron utility process crashes, the OS releases its storage lock. A replacement utility
process creates a new session and calls `open()`; Graft reconstructs the runtime from durable
repository state and may reuse the validated classification snapshot described below. No stale
daemon registration or PID file is involved.

Dropping the JavaScript native object releases its Rust `Arc`, but Eidos should always await
`close()` during orderly Space shutdown so lifecycle errors are observable.

## Incremental status, history, diff, and inventory

`statusIncremental()` fingerprints tracked files and SQLite sidecars with metadata. When HEAD,
index, visible-untracked inventory, and file metadata are unchanged, it reuses the prior status and
does not hash file contents. After a stable classification it atomically writes a content-addressed
snapshot under `.graft/cache/sdk-status`. A new session or replacement utility process can load
that snapshot, then revalidate repository format, HEAD, index, refs/config, the content of all
relevant `.gitignore` / `.graftignore` sources, and current tracked/untracked metadata. Any mismatch,
corrupt/truncated file, or orphan temporary file causes a full rebuild; partial snapshots are never
used. The cache stores no bearer credential or absolute worktree path.

`status.upstream_status` reports the exact Local and Remote-tracking heads plus ahead/behind counts.
When the histories diverge it also reports `common_ancestor`; consumers must stop before pull or
push and preserve both heads instead of selecting a winner implicitly.

`telemetry.persistent_snapshot_hit` is true only when the persisted classification survives all
validation and supplies the returned status. `persistent_snapshot_saved` reports a successful
durable update, and `stability_retries` reports bounded retries caused by concurrent path changes.
`generation` advances only when semantic status changes; `change_token` combines HEAD, generation,
and a semantic status digest, and remains stable across a validated reopen. A full fingerprint
invalidation may rebuild the numeric generation, while the digest still guarantees a different
token for different status. The token should invalidate host snapshots, not
serve as a repository object ID. SDK releases also reject persisted snapshots from an older
classification schema, so changing sniff semantics cannot reuse an obsolete path kind.

Text/binary classification sniffs the first 8192 bytes and reads up to three lookahead bytes when a
valid UTF-8 code point crosses that boundary. Invalid UTF-8 within the sample, a genuinely
incomplete trailing code point, NUL, and disallowed control bytes remain binary. Commit objects are
immutable, so this boundary behavior applies to newly staged snapshots and diffs; it does not
rewrite the recorded kind of an existing checkpoint.

History and remote UI code should not call status merely to obtain repository metadata:

```js
const { current_head, current_branch, upstream, upstream_target } =
  await session.repositoryMetadata({ signal })
const { remotes } = await session.listRemotes({ signal })
```

`upstream_target` is the last locally known remote-tracking commit for the
configured upstream. It lets history UIs decorate the Remote/Cloud tip even
after histories diverge, without fetching.

Both calls read only refs/config metadata, never classify or materialize worktree paths, and report
`telemetry.paths_examined: 0`. Remote entries contain `name`, typed `kind`, and a configured `url`;
HTTP `token_env` and in-memory bearer tokens are deliberately omitted.

`historySummaries({ limit, after })` returns only commit id, parents, message, timestamp, table
counts, and optional path counts. It never reads a commit tree or blob. Commits created before path
counts were added return `path_changes: null` and `path_counts_complete: false`. The `next_cursor`
is the last returned commit id.

When a user opens a commit, `commitChangedPaths({ revision, limit, after })` lazily reads that one
commit and returns a bounded, path-sorted first-parent change inventory. It reads commit trees but
no file blobs. A root commit has `parent: null` and compares against an empty tree. Merge commits
compare against their first parent, matching the summary counts. Page size is at most 100.

Use the returned paths to request only the historical diffs the UI will render:

```js
const page = await session.commitChangedPaths({ revision, limit: 100 })
const paths = page.paths.map(({ path }) => path)
const diff = page.parent
  ? await session.diffPaths({
      paths,
      from: page.parent,
      to: page.revision,
      rows: true,
      limit: 100,
    })
  : await session.diffPaths({ paths, root: page.revision, rows: true, limit: 100 })
```

`diffPaths({ rows: true })` remains the compatibility surface and can return every changed row.
New UI code should use `diffSqlitePaths({ mode: "summary" })` when a database path is expanded,
then `mode: "rows"` only after a table is opened. `rowLimit` is independent of the path `limit`;
`rowAfter` is opaque and must be passed back unchanged. Rowid tables use a leaf-at-a-time streaming
merge and stop after the requested page plus one lookahead change. Native-page-size `STRICT
WITHOUT ROWID` tables with ascending `TEXT` or `INTEGER` primary keys and BINARY collation
use the same bounded model in primary-key order. Other `WITHOUT ROWID` layouts and non-native
SQLite page sizes use a `materialized_compat` path; their response remains bounded but snapshot
materialization can still be proportional to database size. The file and aggregate telemetry
report `rows_scanned`, `rows_returned`, `truncated`, and `response_scope`.
Row cursors are stable for immutable revision comparisons. If the worktree changes while a caller
is paging its live diff, restart from the first page after observing a new status `change_token`.

The focused regression benchmark is configurable so CI can stay small while release validation
can exercise a hundreds-of-megabytes database:

```bash
pnpm --dir packages/graft-sdk bench:sqlite-diff
GRAFT_SQLITE_DIFF_ROWS=1000000 GRAFT_SQLITE_DIFF_PAYLOAD_BYTES=384 \
  pnpm --dir packages/graft-sdk bench:sqlite-diff
```

Set `GRAFT_SQLITE_DIFF_LEGACY=1` only on deliberately small fixtures when comparing the legacy
unbounded response; its purpose is to demonstrate the payload growth that this API avoids.

For a text-file path, read each revision independently and pass the UTF-8 strings to the host's
renderer without exposing `.graft` object paths:

```js
const after = await session.readPathContent({
  revision: page.revision,
  path: "notes/plan.md",
  maxBytes: 1024 * 1024,
})
const before = page.parent
  ? await session.readPathContent({
      revision: page.parent,
      path: "notes/plan.md",
      maxBytes: 1024 * 1024,
    })
  : { content: { state: "absent" } }
```

`readPathContent` accepts one immutable `revision`, one normalized repository-relative `path`, and
a required `maxBytes` between 1 and 8,388,608. The result is
`{ revision, path, kind, storage, content }`; `revision` is the resolved commit id, and `kind` /
`storage` are `null` when the path is absent. `content` is one of:

- `{ state: "absent" }`
- `{ state: "utf8", content, size, content_hash }`
- `{ state: "too_large" | "missing_payload" | "invalid_utf8", size, content_hash }`

The call reads only the selected immutable tree entry and payload, is cancellable, and never
materializes or changes the worktree. Graft resolves inline FileBlob and large-file pointers,
validates their declared kind, size, and content hash, and never returns object paths or
credentials. SQLite paths, invalid revisions, malformed objects, and unsafe limits are rejected
with a structured SDK error. A binary artifact can return `invalid_utf8`; callers should render
text only when `kind === "text_file"` and `content.state === "utf8"`.

`commitDetails(id)` remains available for compatible callers that deliberately need the full
tree-backed commit payload; history lists should not use it.

## Durable merge workflow

The merge API follows Git's durable model rather than creating an SDK-only session record. A true
three-way merge leaves `HEAD` unchanged, records `ORIG_HEAD` and `MERGE_HEAD`, and stores Base,
Ours, and Theirs as index stages 1, 2, and 3. Resolving one path collapses only that path to stage 0.
`getMergeStatus()` therefore survives process restarts and returns `{ state: "none" }` when no
merge is active or the current heads, counts, and an opaque `state_token` while merging.

Always plan before applying:

```js
const head = (await session.repositoryMetadata()).current_head
const plan = await session.planMerge({
  revision: "origin/main",
  ...(head ? { expectedHead: head } : {}),
})

const applied = await session.applyMerge({
  revision: "origin/main",
  ...(head ? { expectedHead: head } : {}),
  planToken: plan.plan_token,
  onProgress: renderTransfer,
})
```

Up-to-date and fast-forward plans finish without an active merge. For a true merge, pass the latest
`state_token` to every paged read and mutation. Any intervening ref, index, worktree, or resolution
change rejects the stale call with `GRAFT_SDK_REPOSITORY_STALE`.

```js
let merge = applied.merge
const page = await session.listMergePaths({
  filter: "unmerged",
  expectedStateToken: merge.state_token,
})

const versions = await Promise.all(
  ["base", "ours", "theirs", "result"].map((version) =>
    session.readMergeVersion({
      path: page.items[0].path,
      version,
      maxBytes: 1024 * 1024,
      expectedStateToken: merge.state_token,
    })
  )
)

const resolved = await session.writeAndStageTextResult({
  path: page.items[0].path,
  content: editedResult,
  expectedStateToken: merge.state_token,
})

await session.continueMerge({
  message: "Merge hosted changes",
  expectedStateToken: resolved.merge.state_token,
})
```

`setMergePathResult` accepts `result: "ours" | "theirs"`. `resolveMergeRow` accepts the same
choice plus a table and stable row identity. With explicit `same_row_merge`,
`resolveMergeCell` selects one structured conflicting column while retaining automatically
combined fields. `resolveMergeTable` atomically applies one choice to
all safely row-resolvable conflicts in a table while retaining both sides' non-conflicting rows;
schema, opaque, and semantic-key conflicts are rejected. `unresolveMergePath` restores the
original stages and worktree conflict candidate for any staged path resolution. Application-edited
SQLite candidates close through the validated `stageMergeSqliteResult` operation. Resolved and
unresolved conflicts remain queryable from the durable merge-session journal until continue or abort.
Like Git's index-first conflict workflow, intermediate row/cell/table choices update only that
journal. They return `worktree_paths: []`; the choice that resolves the final conflict for the path
materializes and stages one complete SQLite candidate and returns that path.
The retained `RepositorySession` caches an immutable SQLite merge plan by Base/Ours/Theirs snapshot
and frozen merge policy. Conflict inspection computes that plan once; later row, cell, and table
choices in the same session reuse it instead of rescanning the complete database.
Candidate construction likewise proves the exact immutable Ours state with a full SQLite
`integrity_check` once per process. Exact content-state hits reuse only that non-forgeable memory
proof; SQLite then applies and validates the transactional delta with native constraints,
`cell_size_check`, index maintenance, and a complete `foreign_key_check`.
On macOS, a proof-backed clean and exclusively locked Ours worktree can seed the private candidate
with an APFS copy-on-write clone. Clone failure and other platforms use the authoritative snapshot
path, so this optimization does not change repository or merge semantics.
When SQLite WAL mode is available, candidate replay retains the committed WAL page numbers and,
after SQLite successfully checkpoints and validates that same WAL, imports only pages that differ
from Ours. Missing, partial, or inconsistent WAL data falls back to the authoritative full import;
WAL pages are never merged across branches or treated as row-conflict semantics.
After the final choice installs and stages a validated SQLite candidate, an exact-token
`continueMerge` commits that state directly. It does not serialize or replace the same database a
second time, and therefore reports `worktree_paths: []` for that completion. The SDK carries the
validated file fingerprints across that ref/index-only commit, so the following status refresh is
metadata-only. It still stats every tracked path and falls back to authoritative classification
after any external write.
Detailed SQLite results are exposed as bounded pages for the selected path. The analyzer computes
the repository conflict set before filtering that page; path-scoped streaming analysis is follow-up
work. The host must validate Eidos File semantics before calling `continueMerge`, then pass the
exact token that was validated.

Applications that own merge semantics beyond Graft's finite resolvers use the generic provider
handoff. Graft exports read-only immutable inputs and never loads application code:

```js
const workspace = await session.prepareSemanticMerge({
  path: "space.eidos",
  provider: "eidos.er-system-merge-1.0",
  managedTables: ["eidos__meta", "eidos__tables", "eidos__fields"],
  expectedStateToken: merge.state_token,
})

// Graft has already seeded result_path from Ours and applied safe Theirs
// changes outside managedTables. The provider reads the immutable inputs and
// updates the application-owned tables in this private candidate.
const outcome = await eidosRuntime.mergeSystemMetadata(workspace)
if (outcome.state === "conflict") {
  await session.recordSemanticMergeConflicts({
    providerToken: workspace.provider_token,
    conflicts: outcome.conflicts,
    automaticResolutions: outcome.automaticResolutions,
    expectedStateToken: merge.state_token,
  })
} else {
  const accepted = await session.acceptSemanticMergeResult({
    providerToken: workspace.provider_token,
    validation: outcome.validation,
    automaticResolutions: outcome.automaticResolutions,
    expectedStateToken: merge.state_token,
  })
  merge = accepted.merge
}
```

The workspace and a recorded conflict outcome survive `close()`/`open()`. The provider token is
bound to the current merge state, frozen policy, immutable revisions, and canonical managed-table
declaration, so a stale or differently scoped result cannot be applied to a changed merge. Graft
refuses to construct the seed when schema/opaque/limited/recomputation-required changes are present
or an unresolved row conflict falls outside `managedTables`; the active merge remains recoverable.
`continueMerge` and `abortMerge` remove the private provider workspace only after their durable
operation succeeds.

Merge policy is a versioned, data-only SDK contract:

```js
const current = await session.getMergePolicy({ signal })
const next = await session.setMergePolicy({
  expectedPolicyToken: current.policy_token,
  signal,
  policy: {
    version: 1,
    same_row_merge: true,
    semantic_keys: { records: ["external_id"] },
    semantic_key_collations: {
      records: { external_id: "nocase" },
    },
    column_resolvers: {
      records: {
        updated_at: "max_timestamp",
        search_text: "recompute",
      },
    },
  },
})
```

The finite managed resolver set is `ignore_for_conflict`, `max`, `min`,
`max_timestamp`, and `recompute`; no resolver executes application code. A
`recompute` candidate remains unresolved with a `recompute_required` validation
artifact. After application-owned recomputation, call
`stageMergeSqliteResult({ path, expectedStateToken, signal })`; Graft captures
the exact SQLite file, validates integrity and foreign keys in a private
database, then stages it. Policy is frozen for an active three-way merge, and
both merge plans/status report the actual policy token/version.

`diffMergeSqlite({ path, from, to, mode, expectedStateToken, signal })` compares any two distinct
immutable versions from `"base" | "ours" | "theirs"` while the merge is active. Summary mode
returns bounded table, schema, and opaque-change facts; rows mode additionally requires `table`
and accepts `rowLimit`/`rowAfter`. Schema entries include `name`, `entry_type`, `op`, `sql`, and
optional `old_sql`. The operation is read-only, is valid with unresolved index conflicts, checks
the merge state token before inspection, supports cancellation, and never materializes the
worktree. A client can compose base-to-ours and base-to-theirs calls into a generic three-way
SQLite view, then apply its own domain-level interpretation.

When one side differs physically but its supported logical SQLite diff from Base is empty, and
analysis reports no schema conflict, row conflict, opaque change, or limitation, Graft may safely
collapse that path to Ours. `listMergeConflicts` reports the same conclusion for an older active
merge as `auto_resolvable: true`, `recommended_result: "ours"`, and reason
`theirs_logically_equivalent_to_base`; clients must not treat unsupported or limited analysis as
logical equivalence.

For working changes, pass `status.status.paths` to `diffPaths`. The API accepts normalized explicit
logical paths, sorts/deduplicates them, and pages them with `limit`/`after`. It never recursively
expands a directory; a tracked file that concurrently becomes a directory is still treated as that
one logical tracked path. The legacy `diff()` remains compatible,
but an unfiltered working diff is now driven by the status change set instead of every tracked path.
Explicit path requests resolve only the matching immutable tree entry and index entry, then read the
one referenced blob. They do not hydrate or clone the full commit maps. Telemetry reports
`path_filter_fast_path: true` and `full_tree_paths_hydrated: 0` for this bounded contract.

`inventory({ kind })` supports `tracked`, `untracked`, `ignored`, and `tracked_ignored`. The last form
is the migration diagnostic for repositories that added ignore rules after committing generated
trees. Ignore rules never untrack an existing path. Review the bounded pages, then pass approved
explicit files to `untrackPaths({ paths, expectedHead })`. It rejects directories, non-normalized
paths, more than 1,000 inputs, and a stale `expectedHead`. Each returned item has its own structured
repository result. The operation removes only index entries: physical files are never deleted or
replaced, `materializes_worktree` is `false`, and files covered by ignore rules remain ignored by
later `addAll` calls. The SDK never performs this migration implicitly.

Explorer-style callers should send up to 1,000 visible entries in one
`isIgnoredPaths({ paths, signal })` call instead of serializing one native task per entry. Each
result includes `is_directory` and `has_tracked_descendants`. An ignored directory with tracked
descendants must remain traversable so the host can expose and migrate those tracked files; an
ignored directory without tracked descendants may be pruned. The batch preserves request order.

The retained session caches the tracked index, compiled nested ignore rules, and the complete
`tracked_ignored` classification. Ignore-source metadata invalidates the matcher and inventory when
a loaded `.gitignore` or `.graftignore` changes. `inventory.telemetry` reports separate inventory,
index, and matcher cache hits; a hot cached page examines zero tracked paths.

Telemetry contains only durations, counts, and cache/object-read facts. It never contains bearer
tokens or absolute user paths.

## Remote transfer progress

`push`, `fetch`, `pull`, `cloneRepository`, and `applyMerge` accept an `onProgress` callback:

```js
await session.push({
  onProgress({ direction, transferredBytes, totalBytes }) {
    const percent = totalBytes
      ? Math.round((transferredBytes / totalBytes) * 100)
      : undefined
    renderTransfer({ direction, transferredBytes, totalBytes, percent })
  },
})
```

Events count cumulative HTTP body bytes for the current operation. `totalBytes` is omitted when
the server does not provide a trustworthy length, so hosts should keep the progress indicator
indeterminate while still showing transferred bytes and a locally calculated speed. Multiple
requests and retries are cumulative; command phases are not transfer percentages.
For `applyMerge`, events cover only snapshot bytes that still need hydration while materializing
the guarded plan. A fully hydrated plan can complete without emitting a transfer event.

## Cancellation, conflicts, and errors

Every asynchronous method accepts `{ signal }`. Aborting still cancels queued Node/libuv work and
also flips a cooperative token for work already running. Status, diff, history, changed-path
hydration, batch ignore queries, stage, untrack, restore, inventory, tree hydration, and SQLite page
loops check that token. Cancellation rejects with an `AbortError`; the retained session remains
open and usable. A cancelled multi-path mutation can leave a completed prefix, but every individual
index write or worktree replacement remains valid. Call status before retrying. `close()` keeps its
existing contract: it waits for the in-flight operation and does not implicitly cancel it.

Repository conflicts and command results retain Graft's existing JSON schema. The binding
stabilizes transport/lifecycle failures as `GraftSdkError` with codes such as:

- `GRAFT_SDK_SESSION_CLOSED`
- `GRAFT_SDK_SESSION_CLOSING`
- `GRAFT_SDK_REPOSITORY_BUSY`
- `GRAFT_SDK_CANCELLED` (normalized to `AbortError` by the JavaScript wrapper)
- `GRAFT_SDK_INVALID_ARGUMENT`
- `GRAFT_SDK_REPOSITORY_STALE` (retryable concurrent ref/index/path-shape change)
- `GRAFT_SDK_REPOSITORY_COMMAND`

Callers should branch on `error.code`, not parse messages.

## Credentials

The SDK uses an explicit, shared, in-memory credential store:

```js
await session.configureRemote({
  name: "origin",
  url: "graft+https://example.com/org/space",
  bearerToken,
})

session.setHttpBearerToken("origin", rotatedBearerToken)
session.clearHttpBearerToken("origin")
```

The token is keyed by remote name and injected into the existing Graft `HttpRemote` only when a
request is built. It is zeroized when its allocation is dropped and redacted from SDK diagnostics.
The SDK rejects HTTP remote URLs containing userinfo, queries, or fragments, and never writes the
token to `config.toml`, a URL, a log, or an error.

SDK sessions do not read `GRAFT_REMOTE_TOKEN` or configured `token_env` values. The CLI retains its
legacy environment-backed behavior as a compatibility path; that policy is not used by the SDK.

## Benchmarks

Build the release CLI and addon, then run:

```sh
cargo build --release -p graft-cli
pnpm --dir packages/graft-sdk bench
pnpm --dir packages/graft-sdk bench:large
```

`GRAFT_SDK_BENCH_ITERATIONS` controls repetition (default 30), and `GRAFT_CLI_PATH` may select a
specific CLI binary. The benchmark reports first/min/p50/p95/max/mean for:

- SDK `open + status + close` cold calls
- repeated hot-session SDK `status` and `diff`
- CLI-process `status` and `diff`

The benchmark deliberately closes the retained SDK session before each CLI sample because the
repository lock correctly excludes a second writer/runtime.

`bench:large` creates a repeatable 46,665-path history, including 46,318 paths that become ignored
only after they were tracked, nested `.graftignore` rules, 51 commits, and one changed `.eidos`
database. It reports cold/hot median and p95 latency, JSON request/response bytes, peak RSS,
cancellation latency, and safe SDK telemetry. The status section explicitly removes only the
rebuildable SDK cache, measures one cold snapshot build, then opens a fresh process for every warm
reopen sample. Set `GRAFT_SDK_LARGE_FIXTURE` to retain/reuse the fixture and
`GRAFT_SDK_LARGE_ITERATIONS` to control repetitions.

The large-repository results and profiler comparison are checked in as
[`benchmark/results/large-repository.md`](benchmark/results/large-repository.md), with the machine-
readable rc5 run in [`benchmark/results/large-macos-arm64.json`](benchmark/results/large-macos-arm64.json)
and the persisted-classification run in
[`benchmark/results/persistent-classification-macos-arm64.json`](benchmark/results/persistent-classification-macos-arm64.json).

The checked-in macOS arm64 baseline is
[`benchmark/results/macos-arm64.json`](https://github.com/eidos-space/graft/blob/main/packages/graft-sdk/benchmark/results/macos-arm64.json). For 30 iterations on
the recorded machine, median hot SDK calls were 1.326 ms for `status` and 1.000 ms for `diff`,
versus 425.436 ms and 429.850 ms for the corresponding CLI process calls (about 321× and 430×).
Cold `open + status + close` remained 420.122 ms median, as expected: the speedup comes from
retaining the repository runtime across calls.

## Release contract

An annotated `graft-sdk-vX.Y.Z` tag on a commit already merged into `main` starts
`.github/workflows/sdk-release.yml`. The workflow builds every advertised target, tests each
binary on Node.js 20 and 24, assembles and verifies all optional packages, publishes platform
packages before the root package, creates a GitHub SDK release from the matching entry in
[`CHANGELOG.md`](CHANGELOG.md) with checksums, then installs the public root package on every
supported platform under Node.js 20 and 24 and exercises `statusIncremental`, metadata, remotes,
history summaries, explicit-path diff, and an up-to-date merge plan/apply cycle. Publishing uses npm
OIDC Trusted Publishing; the SDK workflow does not read a persistent npm token. After each npm
publish, the job allows up to ten minutes for the immutable version to become visible through the
registry read path before it advances to the next package.

See [`RELEASE.md`](https://github.com/eidos-space/graft/blob/main/RELEASE.md) for the first-publish
credential bootstrap, npm trusted publisher configuration, partial-release recovery, and
post-publish checks.
