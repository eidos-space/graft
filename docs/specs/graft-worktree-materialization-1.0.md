# Graft Worktree Materialization 1.0

Status: Draft implementation-aligned normative baseline
Version: 1.0
Published: 2026-08-11
Canonical language: English

## Abstract

Graft versions an application worktree whose SQLite databases are represented
by canonical snapshots in the repository index and commits. The default host
model also presents those snapshots as ordinary physical SQLite files at their
repository-relative paths, so existing SQLite applications can continue to
use normal connections.

This specification defines when Graft may replace, create, or remove those
physical files; how WAL and application handles are quiesced; how the CLI,
Rust SDK, Node SDK, and browser/WASM bridge map to the same behavior; and how
to distinguish worktree materialization from snapshot hydration, temporary
row-diff databases, export, and direct application writes.

The specification intentionally records one conservative gate and one actual
effect model. `operationMaterializesWorktree` means **the operation may
rewrite a tracked worktree**, not that a rewrite will occur for every call.

## Status of this document

The key words **MUST**, **MUST NOT**, **REQUIRED**, **SHALL**, **SHALL NOT**,
**SHOULD**, **SHOULD NOT**, **RECOMMENDED**, **NOT RECOMMENDED**, **MAY**, and
**OPTIONAL** are normative only when written in all capitals. Examples and
implementation notes are informative unless explicitly introduced as a
normative shape, table, algorithm, or conformance requirement.

This version is aligned to the current repository implementation and tests.
It does not pretend that the current filesystem sequence is a cross-path
transaction: individual replacements are atomic, while a multi-path checkout
can require recovery if a later filesystem operation fails. The known gaps are
listed in Section 12.

## 1. Scope, boundaries, and conformance

The ownership boundary is:

```text
CLI / Rust SDK / Node SDK / WASM bridge
                 |
                 v
RepositoryCommandService and RepositorySession
                 |
                 v
Graft core repository: refs, index, snapshots, merge state
                 |
                 v
SQLite worktree adapter: physical files, WAL, locks, replacement
```

The Graft core owns canonical snapshots, commit/path identity, refs, index
stages, merge topology, durable merge state, and stale-head checks. It does
not by itself define whether an ordinary SQLite path is written to disk.

The SQLite worktree adapter owns physical SQLite inspection, consistent WAL
capture, replacement guards, temporary checkout files, sidecar cleanup,
volume bindings, and filesystem recovery. It does not redefine commit or
merge semantics.

The CLI and SDK layers own argument/option validation, operation naming,
session serialization, error mapping, and result projection. The Playground
owns presentation and uses the browser profile; it is not a second merge or
materialization implementation.

An implementation claiming `GWM-Reader-1.0` MUST preserve the non-materializing
guarantees and the distinction between the five non-equivalent worktree/storage
operations in Section 2. `GWM-Writer-1.0` additionally requires the replacement, WAL,
handle, and recovery behavior in Sections 6 and 10. CLI, SDK, and browser
profiles are mappings over those core requirements.

## 2. Terminology and non-equivalent operations

### 2.1 Physical worktree materialization

**Physical worktree materialization** is an operation that creates, replaces,
or removes a tracked file in the repository worktree as a consequence of
Graft changing the checked-out state or resolving a merge. For a tracked
SQLite path this means writing a standalone ordinary SQLite database at the
repository-relative path and, when applicable, updating the Graft volume
binding associated with that path.

The word “materialized” in this specification never means merely that bytes
were read into memory or that a Graft volume was opened.

### 2.2 Canonical snapshot and snapshot/payload hydration

A **canonical SQLite snapshot** is the staged or committed Graft representation
of a database: its page content, page metadata, and repository path identity.
The staged snapshot is the source of truth for `commit`; it is not regenerated
from a possibly newer worktree after staging.

**Snapshot hydration** resolves snapshot pages, objects, or external payloads
into Graft's local runtime/storage so an operation can inspect or apply them.
Hydration MAY use local or remote stores and MAY be proportional to the
requested snapshot. It MUST NOT by itself create, replace, or remove an
application worktree file and MUST NOT be reported as
`materializes_worktree`.

Examples include remote target hydration while planning/applying a merge,
`payload fetch`, audit repair, and loading a commit blob for a historical diff.

### 2.3 `materialized_compat`

`materialized_compat` is a temporary SQLite compatibility database used by the
row-diff engine when direct page/table inspection cannot satisfy the request,
for example because the page size is not native or a `WITHOUT ROWID` table
needs ordinary SQLite execution. It is an internal temporary pair, not a
repository worktree path. Its use MUST be reported as a diff response scope
and MUST NOT be reported as physical worktree materialization.

### 2.4 Export

`graft export` writes a separate ordinary SQLite file chosen by the caller,
from the current worktree or a selected revision. It does not move `HEAD`,
update the index, update the tracked path binding, or count as a checkout
materialization. A caller that deliberately chooses a tracked worktree path
as the export destination has performed an external filesystem overwrite; it
cannot use the non-materializing export classification to bypass the normal
handle/recovery rules.

### 2.5 Direct physical SQLite access

An application, `sqlite3`, or `graft sql` may directly create or modify an
ordinary SQLite file. This is an external worktree edit, not a Graft
materialization. `graft add` later captures a consistent snapshot of that
file, including committed WAL frames, and stages it. The application remains
the owner of its connection and transaction lifecycle.

## 3. Default physical SQLite model

### 3.1 Worktree and canonical identity

The default configuration is:

```toml
[worktree]
materialize_sqlite = true
```

An application SQLite database at `path/to/data.sqlite` is tracked by its
repository-relative path. The commit stores the canonical snapshot descriptor
and path identity, not a promise that the current physical inode will remain
the same after a checkout. A physical rename MUST be communicated through
the explicit SDK `recordPathMove` operation when the host needs to preserve
the tracked path identity without rereading the payload.

The current physical file is authoritative for staging only after Graft has
captured a consistent SQLite image. The staged canonical snapshot is then
authoritative for the next commit, historical diff, merge plan, and checkout.

### 3.2 Configuration semantics

`worktree.materialize_sqlite = true` permits checkout-style operations to write
tracked SQLite snapshots back to their repository-relative physical paths.
This is the default and is the compatibility mode for ordinary SQLite tools.

`worktree.materialize_sqlite = false` suppresses the normal SQLite projection
performed by checkout plans. The repository still updates its canonical state
and Graft volume/path bindings, and tracked non-SQLite artifacts may still be
materialized. This mode is for integrations that intentionally use a
volume-only database path; it does not make an operation safe to classify as
non-materializing, because explicit conflict resolution and other paths can
still write a physical result.

The configuration is therefore a projection policy, not a replacement for the
operation gate. Hosts MUST consult the conservative operation gate first.

### 3.3 Commit boundary

`add`/`stage` captures the physical SQLite image and stores it in the index.
`commit` records the staged snapshot and advances history. `commit` MUST NOT
rewrite the physical SQLite file, even when `materialize_sqlite` is true.
This preserves an open application's file identity and lets the application
continue using its connection after a checkpoint.

The first creation of a physical SQLite file by an application or `graft sql`
is not a commit-time materialization. It already exists before staging.

## 4. Operation lifecycle and gate semantics

### 4.1 Operation classes

Every public operation belongs to one of these classes:

| Class | Meaning | Handle-close requirement |
| --- | --- | --- |
| Non-materializing | MUST NOT create, replace, or remove tracked worktree files | Existing application SQLite handles MAY remain open, subject to ordinary read/write concurrency rules |
| Potentially materializing | MAY change tracked worktree files for the supplied state/path | Host MUST quiesce and close affected application SQLite handles before the call |
| External write | The caller itself writes a file; Graft does not own the write | Caller owns quiesce, atomicity, and recovery |

### 4.2 Conservative gate

The Rust SDK `RepositoryOperation::materializes_worktree` and the Node
`operationMaterializesWorktree(name)` function implement the gate. A `true`
result means only that the operation **can** replace, create, or remove a
tracked physical file for some valid input. It is not evidence that this
invocation changed a file, and it is not an affected-path list.

Hosts MUST close only the affected application database handles when the gate
is true, but SHOULD use the operation's returned path actions to narrow the
set. If a result cannot identify affected paths before execution, the host
MUST conservatively quiesce all application databases that the operation may
touch.

The gate MUST remain conservative across CLI, Node, Rust, and WASM mappings.
An operation MUST NOT be reclassified as false merely because a common input
currently produces no path action.

### 4.3 Lifecycle

For a materializing operation the host lifecycle is:

```text
running application writes
        |
        v
checkpoint WAL / drain transactions
        |
        v
close affected application handles
        |
        v
invoke Graft operation and await settlement
        |
        +--> success: reopen handles and validate expected path state
        |
        +--> error: reconcile status/paths before reopening or retrying
```

The Graft repository session MAY remain open. The application handles, not the
Graft session, are the handles that must be closed before a physical SQLite
replacement.

## 5. Operation matrix

The following table is normative for the current 1.0 surface. “Yes” is the
conservative gate; “actual” describes the normal path for the operation.

| Entry point | Operation | Gate | Normal physical effect |
| --- | --- | ---: | --- |
| CLI / SDK | `init` | No | Creates repository metadata only |
| CLI / SDK | `status`, incremental status, metadata, history, inventory | No | Reads repository/worktree state |
| CLI / SDK | `add`, `addAll`, `stagePaths`, `recordPathMove`, `rm --cached`, `untrackPaths` | No | Captures or changes index state; does not replace files |
| CLI / SDK | `commit` | No | Records staged canonical snapshots; does not rewrite SQLite files |
| CLI / SDK | `diff`, `diffPaths`, SQLite row diff, history content | No | Reads/hydrates snapshots; may use temporary `materialized_compat` only |
| CLI / SDK | `fetch`, `push`, remote config, payload hydration | No | Changes repository/remote storage, not worktree files |
| CLI | `checkout <rev>` or `checkout <rev> -- <path>` | Yes | Checks out the target state; path form scopes the effect |
| CLI | `switch <branch>` / `switch -c <branch> [start]` | Yes | Moves branch and applies its checkout plan; creation at the same state may have no path action |
| CLI | `restore <path>` / `restore --all` | Yes | Restores worktree paths from index, `HEAD`, or a revision |
| CLI | `restore --staged ...` | No | Restores index entries and worktree classification only; does not replace the file |
| CLI | `reset --soft` / `reset --mixed` | No | Changes refs/index classification without checkout |
| CLI | `reset --hard` | Yes | Resets refs and applies a checkout plan |
| CLI / SDK | `pull` | Yes | Fetches and checks out the integrated result |
| CLI / SDK | `clone` / `cloneRepository` | Yes | Creates a repository and checks out the selected branch |
| CLI | `merge <rev>` | Yes | Applies merge outcome and checkout; may leave conflict paths at existing ours state |
| SDK / hidden CLI `merge-api` | `planMerge` | No | Computes up-to-date, fast-forward, or three-way plan only |
| SDK / hidden CLI `merge-api` | `applyMerge` | Yes | Up-to-date normally changes nothing; fast-forward checks out; three-way writes merge state/index and materializes clean paths according to policy |
| CLI / SDK | `setMergePathResult` / `resolve` whole path | Yes | Writes the selected path result and collapses its conflict stage |
| CLI / SDK | `resolveMergeRow` | Yes | Writes the current row-resolution candidate; may materialize a merged SQLite result |
| CLI / SDK | `writeAndStageTextResult` / `resolve --manual` | Yes | Writes and stages the edited physical result |
| CLI / SDK | `continueMerge` / `merge --continue` | Yes | Commits the resolved merge and the current CLI path materialization step may rewrite SQLite snapshots |
| CLI / SDK | `abortMerge` / `merge --abort` | Yes | Restores `ORIG_HEAD` and applies the abort checkout plan |
| CLI | `export` | No (separate output) | Writes only the explicitly selected export destination |
| CLI / SDK | `sql`, application SQLite transaction | No (Graft operation) | Caller directly creates or edits the physical worktree file |

The SDK's current public API does not expose CLI `switch`, `checkout`, or
`reset`; their behavior is still part of the repository worktree contract.
The hidden WASM `merge-api` command constructs a Rust `RepositorySession`, so
it exercises the SDK implementation rather than a browser mock.

## 6. Checkout and replacement algorithm

### 6.1 Plan and preflight

Before changing the checked-out state, the adapter MUST resolve and hydrate
the required canonical snapshots, verify external artifact payloads, preserve
untracked paths, and build the affected-path set. For tracked physical SQLite
paths it MUST run the replacement preflight before changing repository refs or
the index when the command supports that ordering.

The current adapter preflight checks that an existing target is a regular file,
rejects path-type collisions, asks SQLite for an exclusive transaction, and
refuses an active transaction or a WAL that cannot be checkpointed and
detached. Restore also checks untracked collisions; switch/checkout checks
that untracked paths are preserved unless force is explicitly requested.

### 6.2 WAL, locks, and sidecars

For an existing physical database, the adapter MUST:

1. open a read/write SQLite connection with a bounded busy timeout;
2. acquire and release an exclusive transaction probe;
3. if journal mode is WAL, run `wal_checkpoint(TRUNCATE)` and require no busy
   readers/writers and all log pages checkpointed;
4. switch the old database back to rollback-journal mode;
5. remove regular `-wal`, `-shm`, and `-journal` sidecars, rejecting a
   non-regular sidecar; and
6. retain the replacement guard until the filesystem replacement is ready.

A live transaction or an in-use WAL MUST fail the materializing operation
without replacing the main file. Hosts SHOULD close long-lived SQLite
connections before calling; an idle connection is not a substitute for the
host quiesce protocol because a successful replacement changes the directory
entry and file identity.

### 6.3 Per-path replacement

The adapter MUST write a complete standalone SQLite image to a unique temporary
file in the destination directory, flush the temporary output, and atomically
rename/replace the destination under the replacement guard. A temporary
checkout file MUST be removed after a failed write or replacement attempt.

On Emscripten/WASM, the adapter MUST release its old sync-access handle before
the rename when the OPFS/WASMFS implementation requires it; the browser host
must close/reopen its own database handle around the operation.

Each path replacement is atomic at the filesystem operation level. A checkout
of multiple paths is not a cross-path transaction; if a later path fails, the
adapter attempts to restore its staged physical backups and bindings, and the
caller MUST reconcile with `status` before retrying.

## 7. Merge-specific materialization

### 7.1 Plan topology

`planMerge` is read-only and MUST NOT change `HEAD`, index, merge state, or
physical files. It returns one of:

| Plan kind | Condition | Worktree effect during planning |
| --- | --- | --- |
| `up_to_date` | target is an ancestor of current `HEAD` | none |
| `fast_forward` | current `HEAD` is an ancestor of target, or branch is unborn | none |
| `three_way` | both sides have diverged | none; includes merge base, path analysis, and plan token |

Remote target hydration while computing a plan is repository storage hydration,
not worktree materialization.

### 7.2 Apply

`applyMerge` is conservatively materializing because it can change the checked
out state:

- up-to-date revalidates the plan and normally performs no physical write;
- fast-forward moves `HEAD` and applies the target checkout plan; changed
  SQLite paths are projected when `materialize_sqlite=true`, while artifacts
  and other checkout-managed files follow their own projection rules;
- three-way writes the merge index and durable merge state. Non-conflicting
  paths can be checked out. Conflicted paths retain conflict stages and may
  remain at the current ours worktree content until a resolution operation
  writes a result.

The plan token and expected head are compare-and-swap guards. A changed head,
active merge, or token mismatch MUST fail without applying the candidate plan.

### 7.3 Resolution operations

Whole-path ours/theirs selection writes the selected SQLite snapshot or file
artifact to the physical path, updates the volume binding, and collapses the
path to stage 0. A deleted side removes the corresponding materialized path.
Row-level ours/theirs resolution computes a new SQLite snapshot from the
three-way row plan, writes the current candidate to the physical path, and
keeps durable row-resolution state until all row conflicts for the file are
resolved. Text editing writes the supplied UTF-8 bytes to the physical path
and stages the result.

These operations are marked materializing even when a specific path is already
equal to the selected result. The host MUST use the state token returned by
merge inspection and MUST treat a stale token as a retry-from-status event.

### 7.4 Continue and abort

`continueMerge` requires no unresolved conflicts and a valid state token. The
current command path commits the resolved merge and may write the committed
SQLite snapshots back to their tracked paths when `materialize_sqlite=true`.
It returns merge/commit output and affected path actions, but the SDK result
does not yet expose one separate boolean proving that a physical rewrite
occurred.

`abortMerge` requires an active durable merge state and a valid token. It moves
back to `ORIG_HEAD`, clears merge/index conflict state, and applies the abort
checkout plan. It is conservatively materializing even when the target path
set is empty.

## 8. CLI, SDK, Node, and WASM mapping

### 8.1 CLI control plane

The ordinary CLI parses user-facing commands and routes repository commands
through `graft_sqlite::repo_service::RepositoryCommandService`; the service
executes typed `GraftCommand` values against a repository-scoped runtime. The
CLI does not have an independent merge algorithm. The hidden `merge-api`
subcommand additionally opens `graft_sdk::RepositorySession` and calls the
same SDK merge methods used by Node and Playground.

CLI JSON materializing commands expose `paths` or `path_details` where the
command has that projection. Commit's `materialized` array is empty in the
current implementation. Merge continuation may expose a `materialized` array;
merge apply and abort expose path actions but do not expose a universal actual-
materialized boolean.

### 8.2 Rust and Node SDK

Rust `RepositorySession` adds serialized long-lived session lifecycle, expected
head/plan/state token validation, bounded pages, incremental status, and
stable SDK error codes. Node's N-API layer converts JS options to the Rust
types; the published JS package converts camelCase options, JSON, and errors.
None of these layers reimplements snapshot merge or SQLite replacement.

`operationMaterializesWorktree(name)` is the stable host gate. It MUST return
true for `restore`, `restorePaths`, `pull`, `cloneRepository`, `applyMerge`,
`setMergePathResult`, `resolveMergeRow`, `writeAndStageTextResult`,
`continueMerge`, and `abortMerge`; it MUST return false for read, stage,
commit, fetch, plan, merge inspection, history, diff, and path-move APIs.

### 8.3 Browser/WASM profile

The browser cannot load the native Node addon. Playground invokes the WASM
build of the CLI and uses the hidden `merge-api` bridge, which constructs a
real Rust `RepositorySession` in the browser runtime. OPFS/WASMFS handles
have stricter rename behavior: the adapter must release its old sync handle
before replacing a database, and the UI must not claim that a Node-native
connection lifecycle was exercised.

The browser profile MUST disclose this boundary, use the same JSON operation
contract, and preserve durable merge state in the browser repository. A UI
mock that only changes React state is not a conformance test.

## 9. Results, affected paths, and errors

An operation result MAY contain:

```text
paths / path_details: repository-relative path actions
materialized: SQLite or file actions explicitly reported by a legacy CLI path
materializes_worktree: conservative capability bit on batch SDK results
merge: durable merge status after a merge operation
output: the underlying typed command JSON
```

Path actions identify the path, kind/storage where available, and an action
such as `checked_out`, `staged`, `conflicted`, `removed`, or `materialized`.
They describe the command's state transition, not a proof that the host's
filesystem has already flushed every byte. The current SDK contract does not
yet provide a single normalized `affected_paths` plus `actual_materialized`
field for every materializing operation; callers MUST use the operation's
documented output and reconcile with status when exact proof is required.

Errors MUST be surfaced without silently retrying a materializing operation.
Important classes are:

- invalid argument or unsupported path/type;
- active SQLite transaction, live writer, WAL, sidecar, or path collision;
- repository busy due to another session/external writer;
- changed `HEAD`, stale plan token, or stale durable merge state token;
- missing or unhydrated snapshot/payload;
- filesystem failure during a per-path replacement; and
- unknown outcome after a failure crosses a commit or filesystem boundary.

After an unknown or partial outcome, the host MUST reopen/reconcile the
session, query status and merge status, inspect affected paths, and only then
offer retry, continue, or abort. It MUST NOT blindly repeat a checkout.

## 10. Concurrency, durability, and recovery

One retained SDK session serializes operations for one repository. A second
session or an external Graft writer MUST receive the repository-busy error
until the first owner closes or releases its storage lock. External ordinary
SQLite writes are observed by later status/stage operations; they are not
silently merged into an already captured staged snapshot.

Merge state is durable in refs/index metadata (`ORIG_HEAD`, `MERGE_HEAD`, and
index stages). Closing/reopening a session reconstructs merge status from that
state. The host may keep the repository session resident across application
SQLite handle close/reopen, but a crash recovery path MUST reconstruct both
repository state and physical path state before resuming UI actions.

The replacement protocol uses same-directory temporary files and per-path
backup restoration. It does not currently promise a filesystem-wide atomic
multi-path checkout or a durable fsync barrier for every host filesystem. A
conforming host MUST treat status reconciliation as part of recovery.

## 11. Conformance requirements and current evidence

The following families are required for `GWM-Writer-1.0`:

1. **Classification:** assert the complete gate table, including false for
   `commit`, staging, fetch, plan, diff, and merge inspection, and true for
   restore/pull/clone and merge resolution operations.
2. **Commit identity:** open a physical SQLite handle, stage and commit, and
   prove the main file identity and open handle remain valid; prove no commit
   `materialized` list is reported.
3. **Capture:** stage a WAL database without manual checkpoint and prove the
   canonical snapshot includes committed frames while the source WAL is not
   silently destroyed.
4. **Replacement:** checkout/restore/switch a changed database and verify
   content, volume binding, stale sidecar removal, and affected path actions.
5. **Locking:** hold a live transaction or WAL writer and prove replacement is
   refused without replacing the main file; prove new writers are blocked while
   a replacement guard is held.
6. **Merge:** cover up-to-date, fast-forward, clean three-way, conflicted
   three-way, whole-path, row, text, continue, abort, stale plan, and stale
   state token behavior.
7. **Recovery:** inject replacement/write failures, reopen the session, and
   reconcile status without accepting a silently mixed path state.
8. **Profiles:** run the same merge-api contract through Node and WASM/OPFS;
   record the native Node addon boundary explicitly.

Current repository evidence includes:

- `crates/graft-sdk/src/lib.rs` materialization classification tests;
- `packages/graft-sdk/test/repository-session.test.js` ABI/gate, open-handle,
  restore, staging, and identity tests;
- `crates/graft-cli/src/main.rs` CLI commit, switch, clone, and physical-file
  tests;
- `crates/graft-sqlite/src/pragma/sqlite_worktree.rs` WAL, replacement guard,
  sidecar, and physical snapshot tests;
- Rust SDK merge tests for topology, durable status, resolution, stale tokens,
  continue, abort, and reopen; and
- `web-demo/tests/e2e/playground-ui.spec.ts` real WASM merge, resolution,
  abort, and browser UI flows.

## 12. Compatibility notes and known specification drift

The current source, tests, configuration guide, snapshot guide, SDK architecture
guide, and Playground copy establish `commit` as non-materializing. Copies of
those guides from before this 1.0 baseline are stale and MUST NOT override this
specification. One result-shape gap remains:

- CLI JSON and SDK merge result shapes do not yet normalize actual affected
  paths and actual materialization into one field.

The compatibility correction is: direct application/`graft sql` creation makes
the first physical file; `add` captures it; `commit` records it without
replacement; checkout-style operations and explicit merge resolution are the
replacement boundary.

## 13. Rationale (informative)

Keeping `commit` non-materializing protects application SQLite inode identity
and permits a long-lived application handle to remain valid across a version
checkpoint. The conservative gate is still necessary because checkout and
merge resolution can replace the same path. Separating hydration and
`materialized_compat` avoids forcing callers to close application handles for
read-only inspection. Keeping CLI and SDK mappings over one core permits the
browser to exercise the real merge workflow without loading native Node code.

## Normative references

- [Graft SDK TypeScript contract](../../packages/graft-sdk/index.d.ts)
- [Graft SDK materialization table](../../packages/graft-sdk/README.md)
- [Repository merge core](../../crates/graft/src/repo/merge.rs)
- [SQLite checkout and replacement adapter](../../crates/graft-sqlite/src/pragma/repo_checkout.rs)
- [SQLite worktree locking and WAL adapter](../../crates/graft-sqlite/src/pragma/sqlite_worktree.rs)
- [Eidos Specifications index](https://github.com/eidos-space/eidos/tree/main/docs/specs)
