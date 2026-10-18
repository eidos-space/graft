# Graft SDK for Node.js

`@eidos.space/graft` embeds Graft as a long-lived, in-process repository SDK for Node.js. It is the
native application integration surface used by applications such as Eidos Lite. The Rust session
and Node-API crates remain repository-internal implementation crates and are not published to
crates.io.

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
Node.js application
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
platform/architecture-specific. `GRAFT_SDK_NATIVE_PATH` may point tests or packaging harnesses at
an explicit addon path.

Published packages use the standard native optional-dependency model: the root package contains
the JavaScript wrapper, declarations, README, and changelog; each platform package contains exactly
one `.node` binary plus `os`, `cpu`, and (on Linux) `libc` constraints. Consumers do not need a
Rust toolchain or system SQLite library, and the package has no install script.

## Use

Disconnect a cloud connection with `await session.removeRemote("origin")`.
Local files, unsaved changes and local Graft history remain intact; other named
remotes are preserved. The operation is idempotent and does not delete cloud data.
Hosts using an `EncryptedRemoteSession` must revoke and drain that authority first.

```js
const { RepositorySession } = require("@eidos.space/graft")

const session = await RepositorySession.open(spaceRoot, {
  identity: {
    name: "Mayne",
    email: "me@example.com",
  },
})
try {
  const status = await session.statusIncremental()
  const diff = await session.diffPaths({
    paths: status.status.paths.map(({ path }) => path),
    rows: true,
    // Optional: scan just the table the user opened.
    table: "customers",
    limit: 100,
  })

  // For a persistent repository identity, use the config API.
  await session.configSet("user.name", "Mayne")
  await session.configSet("user.email", "me@example.com")
  const configuredEmail = await session.configGet("user.email")

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

Eidos Lite keeps one session per open Space in its resident Node.js process rather than constructing
a session per command:

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

A trusted Node.js process should load this package directly. User-interface processes should receive
typed, serializable results rather than native objects, credentials, or unrestricted paths. The host
must not launch `graft`, run a long-lived CLI daemon, or proxy commands over JSON stdin.

## API and worktree materialization

“Materializes worktree” means that an operation can replace, create, or remove physical tracked
files in the Space. Changes confined to `.graft` are not counted.

| Method | Purpose | Materializes tracked worktree files |
| --- | --- | --- |
| `open`, `close`, `reopen` | Manage the retained repository runtime; `open` accepts an optional in-memory identity override | No |
| `init` | Initialize `.graft` metadata | No |
| `status` | Inspect worktree and index state | No |
| `statusIncremental` | Inspect status with a stable generation/change token and safe cache telemetry | No |
| `repositoryMetadata` | Read head, branch, upstream, and repository format without scanning the worktree | No |
| `listRemotes` | Read a credential-free remote URL/config projection without scanning the worktree | No |
| `configGet`, `configList`, `configSet`, `configUnset` | Read or update effective repository configuration, including `user.name` and `user.email` | No |
| `addAll` | Read/import the current worktree into the index | No |
| `stagePaths` | Stage up to 1,000 explicit paths in one serialized SDK call | No |
| `captureSqliteSnapshot` | Export one immutable SQLite image and optionally a fixed-page delta from a prior opaque capture token | No |
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

`captureSqliteSnapshot({ path, output, baseSnapshotToken?, deltaOutput? })` is the publication
boundary for a live SQLite worktree file. It writes a create-new, standalone immutable image without
staging, committing, moving refs, or replacing the worktree. Supplying the prior opaque token reuses
Graft page storage and, when `deltaOutput` is also supplied, emits a `GRAFTD01` archive containing
the exact base and target byte lengths and SHA-256 digests plus sorted changed 4 KiB pages. The
returned token retains the immutable snapshot identity and digest needed by the next capture. The
caller must treat the token and delta as local temporary data and should still bind publication to
the returned content fingerprint and SHA-256. Output and delta paths must be distinct absolute
paths and existing files are never overwritten.

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

If a Node.js process crashes, the OS releases its storage lock. A replacement process creates a
new session and calls `open()`; Graft reconstructs the runtime from durable
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

`push`, `fetch`, `pull`, `cloneRepository`, `cloneEncryptedRemote`,
`pushInitialBranch`, and `applyMerge` accept an `onProgress` callback:

```js
await session.push({
  onProgress({ direction, transferredBytes, totalBytes, totalIsFinal }) {
    const percent = totalIsFinal && totalBytes
      ? Math.round((transferredBytes / totalBytes) * 100)
      : undefined
    renderTransfer({ direction, transferredBytes, totalBytes, percent })
  },
})
```

Events count cumulative HTTP body bytes for the current operation. A known request length
does not establish the size of the whole operation: show a percentage or remaining time only
when `totalIsFinal` is true and `totalBytes` is present. Otherwise keep the indicator
indeterminate while showing transferred bytes and a locally calculated speed. Multiple
requests and retries are cumulative; a new operation starts fresh counters. Command phases
are not transfer percentages. Upload bytes have been consumed by the HTTP transport;
they do not confirm durable remote storage or successful completion.

E2EE reports ciphertext object uploads (including aggregate framing and multipart parts)
and downloaded response bodies. Dependencies are discovered incrementally, so these events
omit `totalBytes` and set `totalIsFinal` to false. They expose only direction and byte counts,
without plaintext paths, content, keys, or passwords. Initial publication can also report
downloads from its independent verification clone. Local encryption, password derivation,
and final worktree materialization are separate phases and have no transfer percentage.
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
history summaries, bounded exact-path history, explicit-path diff, and an up-to-date merge plan/apply cycle. Publishing uses npm
OIDC Trusted Publishing; the SDK workflow does not read a persistent npm token. After each npm
publish, the job allows up to ten minutes for the immutable version to become visible through the
registry read path before it advances to the next package.

See [`RELEASE.md`](https://github.com/eidos-space/graft/blob/main/RELEASE.md) for the first-publish
credential bootstrap, npm trusted publisher configuration, partial-release recovery, and
post-publish checks.

### Exact-path history

```js
const page = await session.pathHistory({
  path: 'notes/example.md', limit: 50, maxCommits: 100,
  maxBytes: 8 * 1024 * 1024, // metadata read budget, not response size
  // cursor: previousPage.next_cursor,
  // signal: abortController.signal,
})
// Request another page only on a bounded user/application action.
// Empty commits + has_more means more history remains to scan.
// Read a selected version using readPathContent({ revision: entry.id, ... }).
```

This is exact-path first-parent history, including deletion/recreation; renames
are not followed. A cursor pins the initial HEAD and keeps working after HEAD
changes, as long as required immutable objects still exist. Never edit cursors
or reuse them for a different path. `has_more: false` is the end of traversal;
an empty page alone is not. Limits are 1–100 matches, 1–1000 comparisons and
1 KiB–64 MiB actual metadata bytes. Defaults are 50, 100 and 8 MiB. If one
comparison cannot fit, increase `maxBytes`; comparisons exceeding the hard
ceiling explicitly fail. No content blobs or worktree materialization occur.

`restorePaths({paths:[path], source:entry.id, expectedHead, requireClean:false})`
restores the worktree while preserving HEAD and the complete index, including
staged changes to the target. Other dirty and untracked files remain unchanged.
`requireClean:true` rejects changes anywhere in the tracked repository.
`expectedHead` is not a compare-and-swap of target bytes: external target edits
can be overwritten. An integrating host must coordinate its writers, recheck
target bytes/existence and staged state against the user's reviewed state, and
keep writers quiescent through completion. A separate check alone has a TOCTOU
window; Graft does not provide an atomic filesystem-content CAS. Unsaved editor
drafts and copies remain the host's responsibility.

Ordinary-file replacement is an atomic rename, but worktree bookkeeping follows
replacement and may fail after the file changed. Multi-path restore is not a
transaction; a failure/cancellation can leave an applied prefix. After any
mutation error, re-read actual state before retrying. A directory path is a
pathspec and can affect descendants; use an exact file path for single-file UI.

## Native encrypted remote hosts

Trusted Node hosts can opt into the native `e2ee` feature. Default native builds
do not include it. The build script's third argument selects `plain`, `e2ee` or
`macos-keychain`, after its target and output directory. `pnpm test:e2ee-node`
builds a temporary addon and verifies it against the actual HTTP remote handler.
Native checkpoint storage supports Unix owner-only modes and Windows protected
owner/SYSTEM-only DACLs outside the repository. Use
`EncryptedRemoteSession.createPrivateDirectory(absolutePath)` to allocate each
new checkpoint, recovery and staging directory. Existing directories are checked,
never silently made less restrictive. Windows supports password-based session
unlock; remembering keys requires an approved host provider.

For an existing encrypted remote, first authenticate the account and verify its
domain/subject and pinned UUID. Create a host handle with explicit private-state
paths. Use `createCheckpoint: true` only for intentional new-device enrollment;
use `false` on later starts so missing/corrupt state is rejected.

```js
const { EncryptedRemoteSession, RepositorySession } = require('@eidos.space/graft')
const authority = new EncryptedRemoteSession({
  remoteName: 'origin', url: verifiedRemoteUrl, uuid: pinnedRepositoryUuid,
  authenticationDomain: verifiedDomain, accountSubject: verifiedAccount,
  bearerToken: accountToken,
  checkpointDirectory: privateCheckpointDirectory,
  createCheckpoint: isNewDeviceEnrollment,
  stagingDirectory: privateStagingDirectory,
  recoveryDirectory: privateRecoveryDirectory,
})
await authority.unlock(passwordBuffer) // Consumes and immediately clears this Buffer.
const session = await RepositorySession.open(target, { encryptedRemote: authority })
await session.addEncryptedRemote() // Explicit add; does not overwrite an existing remote.
await session.push({ remote: 'origin', branch: 'main' })
await authority.lock() // Cancels and drains key-owning work before resolving.
await session.close()
await authority.disconnect()
```

For a fresh destination, use `session.cloneEncryptedRemote({ branch: 'main' })`
instead of initializing/adding a remote. The destination must be empty or absent.
The clone retains the authority's `remoteName`, including names other than `origin`,
and shares its live token refresh, lock and disconnect lifecycle. Passwords must be exclusive caller-owned
Buffers containing 1–1024 UTF-8 bytes; strings and invalid input are rejected.
The API does not erase copies previously created by the application. Dropping a
Promise does not cancel a request: explicitly lock or disconnect it. Runtime
close/reopen leaves the shared authority's state intact.

Keep `recoveryDirectory` across process restarts, separate from disposable staging
and outside the worktree. Binding an encrypted repository session rejects
checkpoint, recovery and staging directories that overlap its discovered
worktree or repository metadata, including resolved path aliases. The same check
runs when adding or cloning an encrypted remote. Checkpoint and recovery paths inside staging are rejected
before enrollment. Native ref updates persist their exact ciphertext before
publication. A new handle over the same checkpoint and recovery directories
reconciles pending updates and retries the retained bytes. Unknown results remain
pending; a different target cannot replace an unconfirmed candidate. Missing
records/directories in admitted slots, damaged records, or missing/damaged
namespace allocation evidence fail closed. `createCheckpoint` controls explicit first enrollment of both
checkpoint and recovery storage; `false` reopens both and never recreates missing
storage. Reconnect keeps the same recovery providers. Retain the entire directory,
including its `multipart` namespace and any pending upload ciphertext. A missing
multipart namespace in an existing enrollment is a recovery error and is never
silently recreated.
After explicit unlock, `authority.retainedUploads()` lists admitted unfinished
uploads by opaque key. `resumeRetainedUpload(key)` reopens the exact retained
ciphertext. If the server's multipart admission cannot be recovered, explicitly
call `restartRetainedUpload(key)`: native code confirms abort of the pinned
server session before starting a new one with the same ciphertext. An unknown
abort preserves the old identity. These methods cannot allocate a missing slot
or accept arbitrary recovery paths. Their results are `created`, `reused`,
`unconfirmed` or `unknown`; only the first two confirm authenticated completion.
Interrupted empty first allocation resumes only when durable evidence proves the
slot has never been admitted. Existing active or corrupt records are never
overwritten. Reconstruction of lost admitted slots or namespace evidence is
not yet supported.

`pause(true)` retains the key while preventing sync admission. Reconnect requires
the same verified account identity and remains locked until explicit unlock.

For a newly account-registered empty encrypted remote, a locked authority can
call `authority.initializeConfig(passwordBuffer, { branch, commit })`, using the
source repository's committed branch and HEAD. The native entry clears the
exclusive caller-owned Buffer synchronously, generates a random root and writes
one locally verified wrapped candidate and the source intent in `recoveryDirectory` before publishing
it create-if-absent. No root key or config bytes pass through JavaScript. Existing
local config trust prevents a replacement initialization.

The result is `config_published`, `unconfirmed`, `conflict` or `unknown`.
`config_published` confirms only the config; initial source/branch/HEAD publication
and readable-branch verification are still required for connected success.
For an uncertain result, await `authority.lock()` or reopen the same host
directories with `createCheckpoint: false`, then call
`authority.resumeInitialConfig(passwordBuffer)` while locked. Resume unwraps the
retained candidate, inspects it and retries its original bytes only when still
absent. Wrong passwords or lost records fail closed. Conflict locks the losing
session; the host must explicitly enroll in the winner using fresh local trust
rather than reuse the losing root or overwrite its config. Empty remote identity
registration is an account-service responsibility outside these SDK APIs.

After config publication, an unlocked authority-bound repository can call
`source.pushInitialBranch()`. Native recovery requires the checked-out branch
and committed HEAD to match the retained initialization intent. Missing intent
stops publication; config-only initialization cannot reconstruct it later.
Retries and reopened sessions load that same intent; another source commit is rejected before network discovery.
The repository service rechecks the source before dependency publication and
uses the retained ref/HEAD recovery slots. A failed HEAD update does not replace
the intent or upload a different initial history.

For the connection gate, call
`source.pushInitialBranch({ verificationDirectory: freshEmptyDirectory })`.
Use a fresh independent destination outside the source repository and all host
checkpoint/recovery/staging directories. Occupied or overlapping destinations
are rejected before initialization intent or remote publication changes. An
absent destination is allowed. Native code clones the default remote
branch into it, verifies the expected branch and commit, closes the verifier and
durably advances the initialization journal to Connected. Only then does the
result contain `state: "connected"`; without verification it contains
`state: "branch_published"`. The verification directory contains ordinary
plaintext local files and is left available to the host. Each retry needs a
fresh destination; the SDK does not erase an existing verification copy.
After reopening a branch-publication attempt, use ordinary `unlock` on the
authority and reopen the original repository before retrying this operation.
The persisted source remote must exactly match the authority's config. Custom
remote names are supported: verification derives an isolated `origin` credential
registration for the same pinned remote and secret session, without renaming or
modifying the source authority's registrations.

Existing committed history can migrate to a newly registered encrypted target
with `source.prepareEncryptedMigration(passwordBuffer, { allRefs: true, mappings,
targetHead })`. Each mapping contains full `source` and `target` ref names; every
selected branch, unborn branch, remote-tracking ref and tag needs an explicit mapping. Alternatively,
pass a nonempty `references` array instead of `allRefs`. `targetHead` is a full
mapped branch name, such as `refs/heads/main`. Preparation is offline and retains
one wrapped root plus exact source, target, enrollment and progress journals
under the authority's recovery directory.

`resumeEncryptedMigration(passwordBuffer)` publishes only that retained plan.
It can be repeated after reopening the same authority with `createCheckpoint:
false`; missing state never replans or generates another root. Publication
returns `ready-for-independent-verification`. Then call
`verifyEncryptedMigration(passwordBuffer, { verificationDirectory, bearerToken })`
for fresh password recovery of every selected ref and all reachable SQLite and
file history. Use a fresh empty or absent absolute destination outside the
source and host directories, and a token for the same verified account.

`cutoverEncryptedMigration(passwordBuffer, verificationOptions)` repeats that
independent verification while native code holds the source's actual canonical
repository mutation gate, then atomically persists the local remote and mapped
branch upstreams. The result is `host-cutover-confirmed`. These migration calls
require a locked authority, clear the supplied exclusive Buffer synchronously,
and relock before completion. No JavaScript boolean or callback can stand in for
the native fence. Every verification attempt needs a fresh destination; its
plaintext recovered history remains available to the host.

The gate excludes participating current-version Graft commands in other threads
and processes for this local repository. Hosts must separately coordinate their
ordinary SQLite/file writers; direct `.graft` edits and legacy binaries are not
covered. Migration changes committed history and local configuration only.
`offlineDevices` remains `pending`; local cutover never claims device revocation.

Trusted hosts can call `authority.changePassword(currentBuffer, newBuffer)` while
unlocked. Both exclusive caller-owned Buffers are cleared synchronously, including
rejection. Current-password reentry authorizes the change; the native provider
persists the exact config transaction inside `recoveryDirectory`. No JavaScript
crypto or journal callbacks are required.

The result is `confirmed`, `unconfirmed`, `conflict` or `unknown`. Display completion
only for `confirmed`. A pending transaction prevents preparing another change.
After an uncertain result, explicitly unlock the appropriate current/new password
or approved remembered key, then call `authority.resumePasswordChange()` on the
same or reopened authority. It restores retained bytes, inspects the result and
retries only when the exact old config remains current. Unknown outcomes require
reconciliation; conflicts require the host to stop and refresh its flow. Existing
unlocked devices continue using the same root key after a password change.

The native journal retains the last retired candidate until a newer transaction
supersedes it. This lets resume recover a lost local retirement acknowledgement
after reopening. A retired record alone does not establish success; resume still
authenticates the remote config. The record contains password-wrapped root
material, so changing a password does not revoke access to older wrapping records.
This API does not implement forgotten-password reset or device revocation.
Switching accounts requires a new handle. Refresh tokens only after verifying
their unchanged domain/subject with `refreshBearerToken`.

Handle encrypted repository failures through `GraftSdkError.code`, including
errors from `push`, `fetch` and `cloneEncryptedRemote`:

| Code | Action |
| --- | --- |
| `GRAFT_SDK_E2EE_LOCKED` | Ask for explicit unlock. |
| `GRAFT_SDK_E2EE_PAUSED` | Resume only according to the host's user intent. |
| `GRAFT_SDK_E2EE_AUTHENTICATION_FAILED` | Resolve HTTP account authentication/authorization; verify identity before refreshing a token. |
| `GRAFT_SDK_E2EE_INTEGRITY_FAILED` / `GRAFT_SDK_E2EE_CONFIG_ROLLBACK_DETECTED` | Stop and investigate; do not automatically overwrite data or reset the checkpoint. |
| `GRAFT_SDK_REMOTE_PUBLICATION_OUTCOME_UNKNOWN` | Reconcile the publication before deciding whether to retry. |

Account authentication failure and encrypted-data integrity failure are separate
conditions. Error text is diagnostic; do not use it to decide whether a repository
is busy or a secret session needs unlock. An unknown/unconfirmed publication
retains its publication code even when its underlying cause is cancellation.

A `macos-keychain` build accepts `keychainApplication` and
`keychainAccessGroup` for a correctly signed/entitled host's default application
group. `remember`, `unlockRemembered` and `forget` use that native provider.
Unavailable storage disables remembering without a plaintext fallback; a host
may continue with session-only password unlock. Forget fences and drains all
process-local sessions sharing the binding, including through caller cancellation.
Those handles require explicit reconnect and unlock afterward. Hosts sharing an
entry across processes must coordinate those processes. Successful remembering/restart in a signed
host still requires platform validation. Keep this authority in the trusted host;
never expose it or its key-management methods through ordinary plugin RPC.

These Node APIs do not provision the account-side encrypted service identity,
reset a forgotten password or claim revocation of offline devices. Config
initialization, migration of committed history, native fenced local cutover and
current-password change are
described above. Remaining workflows and production conformance gates are defined
in [E2EE Remote 1.0](../../docs/specs/graft-e2ee-remote-1.0.md).
