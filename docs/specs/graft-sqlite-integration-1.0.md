# Graft SQLite Integration 1.0

Status: Draft implementation-aligned normative baseline
Version: 1.0
Published: 2026-08-11
Canonical language: English

## Abstract

This specification defines the Graft SQLite extension and live VFS data plane:
extension configuration/registration, opening or importing a database as a
Graft volume, page I/O, lock transitions, snapshot freshness, coordination with
repository checkout, secondary files, SQLite error mapping, and the production
PRAGMA boundary.

This integration is different from physical worktree materialization. The VFS
serves a live SQLite database from Graft storage; materialization projects a
committed snapshot to an ordinary filesystem database for non-VFS applications.

## 1. Scope and architecture

```text
SQLite connection (`vfs=graft`)
        |
        v
GraftVfs / VolFile ----- diagnostic volume PRAGMAs
        |
        v
Runtime -> VolumeReader / VolumeWriter -> page/log storage

CLI / SDK -> RepositoryCommandService  (separate control plane)
```

The VFS owns live SQLite file semantics, lock upgrades, page reads/writes, and
volume binding. The repository service owns status, stage, commit, history,
branch, merge, and remote operations. Production repository commands MUST NOT
be tunneled through SQLite PRAGMAs.

Storage page semantics are defined by [Graft Storage and Snapshots
1.0](./graft-storage-snapshots-1.0.md). Ordinary-file checkout and application
handle quiescing are defined by [Graft Worktree Materialization
1.0](./graft-worktree-materialization-1.0.md).

## 2. Extension configuration and registration

The extension reads optional `graft.toml` relative to the host process's current
working directory at initialization. Its configuration fields are:

| Field | Meaning |
| --- | --- |
| `remote` | base runtime remote configuration |
| `data_dir` | persistent local Graft storage; absent means temporary storage |
| `log_file` | append tracing to this file instead of SQLite logger |
| `make_default` | register `graft` as SQLite's default VFS |
| `autosync` | optional non-zero synchronization interval in seconds |

Invalid config, non-UTF-8 config path, runtime setup failure, or registration
failure MUST fail extension initialization with a SQLite error and message.

Dynamic loading registers the VFS name `graft` and returns permanent-load
success. Static hosts call the exported static initializer, which registers the
same VFS behavior. A process may load the extension for multiple connections;
the first successfully installed global tracing subscriber wins, and later
loads MUST NOT panic merely because tracing is already initialized.

Credentials follow the selected runtime/adapter policy and MUST NOT be logged.

## 3. Runtime selection and tags

A normalized main-database path is the VFS tag. If the path belongs to a
discoverable Graft repository, the VFS uses a runtime fork rooted at that
repository's storage directory. Runtime instances are cached per canonical
`.graft` directory. Otherwise it uses the configured base runtime.

Tag/path normalization MUST reject invalid forms and yield one stable lookup
key. VFS `access` reports a main database as present when either a volume tag or
a recognizable physical SQLite file exists.

## 4. Main database open

For a main database, open follows this ordered decision:

1. if the normalized tag already names a volume, open that volume;
2. otherwise, if a physical SQLite database exists, import it as a new volume
   and bind the tag;
3. otherwise, if SQLite requested create, create a new volume and bind it;
4. otherwise, return `SQLITE_CANTOPEN`.

Read-only mode MUST NOT later upgrade to a writer. A create request does not
silently replace an existing incompatible physical file.

### 4.1 Physical VFS import

A file qualifies for import only when it has a readable SQLite header. Import
requires SQLite page size exactly 4096 bytes, file length an exact multiple of
4096, and page count representable by the storage model. Every page is read in
order, validated as a 4096-byte Graft page, written to a new volume, committed,
and only then published under the tag.

Failure before tag publication MUST not expose a partially imported volume as
the named database. Import is a VFS bootstrap path; it is not repository
`stage`, snapshot hydration, or worktree materialization.

The current VFS importer reads the main database file directly. It does not run
SQLite online backup or merge a sibling `-wal`, so callers MUST close/checkpoint
an ordinary SQLite writer before first VFS import when committed WAL frames may
exist. This is intentionally weaker than repository staging's consistent WAL
capture. The source physical file is not removed or kept as a live mirror; once
the tag exists, `vfs=graft` opens the volume first, while a non-VFS application
opening the ordinary file can observe or diverge from the old bytes.

## 5. Secondary and transient files

Only `MainDb` opens a volume-backed `VolFile`. Journals, temporary databases,
and other secondary SQLite files use isolated in-memory files. VFS delete for
those paths is an idempotent no-op because their memory is released on close.

The implementation MUST NOT accidentally bind a journal/WAL/temp filename as a
durable Graft volume. Host process lifetime therefore bounds these secondary
files; callers must not assume they survive a crash as ordinary filesystem
sidecars.

## 6. File and page I/O

Logical file size is `4096 * page_count`. Read, write, and truncate translate
between SQLite byte offsets and Graft pages with explicit bounds checking.

A single VFS read or write MUST NOT cross a 4096-byte Graft page boundary.
Writes and truncate require the Reserved/writer state. Truncate size MUST be an
exact multiple of 4096 and follows storage soft-truncate semantics. Short reads
and invalid ranges map through normal SQLite VFS expectations; storage errors
must not be disguised as valid zero pages.

The VFS advertises these device characteristics:

```text
atomic writes through 4 KiB
powersafe overwrite
safe append
sequential write ordering
```

An implementation claiming them MUST preserve the corresponding observable
SQLite guarantees through its volume commit path.

### 6.1 SQLite file-change fields

SQLite page 1 contains the file change counter and version-valid-for number.
When a full first-page write differs only in those fields, the VFS may ignore
the write to avoid producing a new storage snapshot for connection bookkeeping.
On read, it synthesizes a change counter from the current snapshot identity so
SQLite can detect a changed database image.

Ignoring those fields MUST NOT ignore any other page-1 byte change. Snapshot
hash/counter mapping need only be stable enough for SQLite cache invalidation;
it is not repository object identity.

## 7. Lock and transaction state machine

The internal state is:

```text
Idle --Shared lock--> Shared(reader)
Shared --Reserved lock--> Reserved(writer)
Reserved --unlock to Shared--> Committing --> Shared(new reader)
Shared --unlock--> Idle
```

`Pending` and `Exclusive` lock requests are valid only while already Reserved;
they do not create a second writer state.

### 7.1 Shared

On `Idle -> Shared`, the handle refreshes its volume binding and opens a stable
`VolumeReader`. Subsequent reads observe that snapshot until the lock cycle
changes. An invalid duplicate transition fails.

### 7.2 Reserved

`Shared -> Reserved` requires all of:

1. the file is writable;
2. the per-tag Reserved mutex is available;
3. the Shared reader snapshot is still the latest volume snapshot; and
4. the workspace coordinator permits a VFS writer.

Mutex or workspace contention returns `SQLITE_BUSY`. A newer committed
snapshot returns `SQLITE_BUSY_SNAPSHOT`; the connection must restart its SQLite
transaction instead of overwriting the newer state.

After all checks, the reader becomes a `VolumeWriter`, and the handle owns both
the per-tag writer guard and workspace-writer count until commit/abandon.

### 7.3 Commit/downgrade

Unlock from Reserved to Shared attaches any pending storage message, commits
the `VolumeWriter`, installs a reader for the new snapshot, releases writer
guards, then marks the repository-relative path dirty when it belongs to a
repository. Repository dirty bookkeeping can fail after the SQLite storage
commit; that condition MUST be reported and remain recoverable by later status
or staging rather than rolling back an already durable volume commit.

If commit fails, state enters a recognizable committing/abandon path. SQLite's
subsequent unlock to Unlocked releases leaked writer guards and returns Idle.
Invalid lock/unlock transitions fail rather than corrupting the state machine.

## 8. Workspace coordination

One `WorkspaceCoordinator` prevents live VFS writers and repository checkout/
materialization from running concurrently in the same VFS/runtime context.

- checkout acquires an exclusive checkout flag only when writer count is zero;
- a writer increments its count only after confirming checkout is inactive and
  rechecks the flag to close the race; and
- releasing either guard restores availability.

Failure to acquire this gate returns busy. This in-process gate complements,
but does not replace, application handle quiescing and filesystem replacement
locks required by the materialization specification.

## 9. PRAGMA surface

### 9.1 Stable production boundary

The only non-debug informational PRAGMA is:

```sql
PRAGMA graft_version;
```

Repository PRAGMAs such as `graft_status`, `graft_add`, `graft_commit`, branch,
merge, and remote commands are removed from production. They return an error or
not-found result directing callers to CLI/SDK control-plane operations. A
feature-gated legacy test constructor MAY enable them only for compatibility
tests; production extension registration MUST use the normal constructor.

### 9.2 Diagnostic volume controls

Current `graft_debug_*` families include:

- volume info/status/list/tags/snapshot/header and log/table-log inspection;
- volume new/switch/clone/fork/checkout-LSN/reset/message;
- volume fetch/pull/push/audit/hydrate/export;
- raw LSN show/diff/commit dump and volume page/row diff.

These operate on volume IDs, logs, LSNs, pages, and storage commits, not
repository branches/commits. They are intentionally unstable diagnostics and
MUST NOT be used as the application control contract. Deprecated volume import
returns guidance to use supported SQLite mechanisms such as `VACUUM INTO`.

Unknown PRAGMAs MUST return SQLite not-found/error behavior rather than execute
an approximate command.

## 10. Error mapping

The VFS maps errors as follows:

| Condition | SQLite result |
| --- | --- |
| unknown PRAGMA | `SQLITE_NOTFOUND` |
| missing tag/non-creatable main DB | `SQLITE_CANTOPEN` |
| writer/workspace contention | `SQLITE_BUSY` |
| stale reader/concurrent volume write | `SQLITE_BUSY_SNAPSHOT` |
| cooperative cancellation | `SQLITE_INTERRUPT` |
| local storage/remote I/O failure | `SQLITE_IOERR` |
| invalid transition, recovery-required, divergence, other internal failure | `SQLITE_INTERNAL` or explicit PRAGMA error |

Specific SQLite errors MUST be preserved through the extension boundary.
Messages may add context but must redact credentials and avoid claiming a
transaction was rolled back when a storage commit already completed.

## 11. Crash, close, and recovery

A stable Shared reader remains immutable. A successful writer commit publishes
one new storage snapshot. A process crash before commit loses in-memory
secondary files and uncommitted writer state; a crash after storage commit may
leave repository dirty bookkeeping to be rediscovered.

Closing a VFS handle releases its lock-manager reference. The per-tag lock may
be removed when no handle references it. Volume deletion-on-close is not a
supported durable deletion contract in version 1.0.

Repository checkout must not infer safety merely because no VFS object is
visible in another process. Hosts using ordinary files must still close/reopen
application connections around materializing operations.

## 12. Conformance requirements

A `GRAFT-VFS-1.0` implementation MUST test:

1. dynamic/static registration and valid/invalid `graft.toml` behavior;
2. tag normalization and per-repository runtime selection;
3. existing-tag, physical-import, create, and cannot-open branches;
4. 4096-byte page/file validation, documented main-file-only WAL boundary, and
   no partial tag publication;
5. in-memory secondary files without durable accidental tags;
6. read/write/truncate boundaries and first-page field handling;
7. every valid and invalid lock transition;
8. busy versus busy-snapshot distinction;
9. writer commit, dirty marking, and failure/abandon recovery;
10. workspace checkout/writer exclusion races;
11. production PRAGMA removal and diagnostic-only volume behavior; and
12. SQLite error-code preservation.

Current evidence lives in `crates/graft-sqlite/src/vfs.rs`,
`crates/graft-sqlite/src/file/vol_file.rs`, PRAGMA parser/evaluator tests,
workspace/checkout tests, and dynamic/static extension tests in `crates/graft-ext`.

## 13. Known limits

- Live VFS import/write requires 4096-byte SQLite pages.
- Initial physical VFS import reads only the main file, does not reconcile WAL,
  and does not maintain the source file as a live mirror.
- Secondary SQLite files are in-memory and do not survive process failure.
- `graft_debug_*` is not stable application API.
- The workspace coordinator is in-process; cross-process physical-file safety
  requires the separate materialization protocol.
- Repository dirty marking can fail after a durable storage commit and is
  recovered by repository inspection rather than undoing the SQLite commit.
