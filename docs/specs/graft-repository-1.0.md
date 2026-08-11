# Graft Repository 1.0

Status: Draft implementation-aligned normative baseline
Version: 1.0
Published: 2026-08-11
Canonical language: English

## Abstract

This specification defines Graft repository discovery, on-disk layout,
canonical object representation, path identity, refs, index stages, tracked
artifacts, status, history, configuration, and repository-level maintenance.
It is the identity and metadata layer used by storage snapshots, diff, merge,
remote synchronization, and host adapters.

It does not specify the internal Fjall key layout, SQLite page storage,
physical worktree replacement, remote wire transport, or UI presentation.

## 1. Scope and ownership

The repository layer owns:

- repository format discovery and validation;
- normalized repository-relative path identity;
- content-addressed blob, tree, commit, and annotated-tag objects;
- `HEAD`, branch, remote-tracking, and tag refs;
- index stages, tracked/untracked inventory, status, and history metadata;
- repository configuration and ignore/track-root policy; and
- reachability roots used by object and payload maintenance.

Page/log storage is defined by [Graft Storage and Snapshots
1.0](./graft-storage-snapshots-1.0.md). Logical comparison is defined by
[Graft Diff 1.0](./graft-diff-1.0.md), merge state by [Graft Merge
1.0](./graft-merge-1.0.md), and physical projection by [Graft Worktree
Materialization 1.0](./graft-worktree-materialization-1.0.md).

## 2. Repository discovery and layout

### 2.1 Repository root

A Graft repository is a worktree containing a `.graft` directory. `init`
MUST canonicalize the worktree root, create the required layout, write default
configuration, and create an unborn `HEAD` pointing to the default branch.

`open` MUST validate the repository format and object format. `discover` MUST
walk from a supplied directory toward its ancestors until it finds `.graft`.
Discovery for a file path MUST begin from the file's containing directory.
Failure to find or validate a repository MUST be explicit; implementations
MUST NOT silently initialize one.

### 2.2 Format markers

The current repository format is:

```text
repository format version = 2
object format             = blake3
object envelope version   = 1
```

An implementation MUST reject unsupported repository or object format values.
Changing either persisted format requires a versioned compatibility rule.

### 2.3 Required paths

The implementation uses these repository paths:

```text
.graft/config.toml
.graft/HEAD
.graft/MERGE_HEAD
.graft/ORIG_HEAD
.graft/refs/heads/
.graft/refs/remotes/
.graft/refs/tags/
.graft/logs/HEAD
.graft/logs/refs/
.graft/objects/
.graft/objects/pack/
.graft/store/fjall/
.graft/store/files/
.graft/index/state.toml
.graft/index/worktree.toml
.graft/locks/
.graft/tmp/
```

`MERGE_HEAD` and `ORIG_HEAD` are conditional durable merge records. Cache and
temporary files MAY be added without changing the format when they are
reconstructable and do not participate in object identity.

### 2.4 Repository paths

A repository path MUST be normalized UTF-8, relative to the worktree root,
and contain no empty, `.` or `..` components. It MUST NOT address `.graft` or
escape the worktree. Equivalent platform spellings MUST map to one canonical
repository path before object construction or index comparison.

## 3. Canonical objects and identity

### 3.1 Envelope and object ID

Every loose canonical object has this byte representation:

```text
graft-object 1 <kind> <payload-length>\0<payload>
```

`<kind>` is `blob`, `tree`, `commit`, or `tag`. `<payload-length>` is the
payload byte length. The object ID is the 32-byte BLAKE3 digest of the complete
canonical envelope, rendered as 64 hexadecimal characters. Reads MUST verify
that bytes hash to the requested ID. Loose-object storage MUST use the first
two hexadecimal characters as fanout and the remainder as the leaf name.

Canonical writers MUST be deterministic. Readers MAY accept documented legacy
payload encodings but MUST emit the current canonical encoding when rewriting.

### 3.2 Blob payloads

The current blob families are:

| Format | Purpose | Canonical notes |
| --- | --- | --- |
| `sqlite-snapshot-v1` | SQLite page snapshot descriptor | volume ID, page count, ordered log ranges and expected commit hashes |
| `file-blob-v2` | inline file bytes | Base64 payload; current writer format |
| `file-blob-v1` | legacy inline file bytes | Base58 accepted for compatibility |
| `large-file-pointer-v1` | external payload pointer | payload kind, BLAKE3 content hash, byte size |

A SQLite snapshot descriptor MUST validate its page count, ordered ranges,
range boundaries, and one expected storage commit hash for every represented
LSN. It identifies canonical storage state; it is not the physical database.

An external pointer MUST identify payload content independently of repository
object identity. Missing payload bytes do not invalidate the pointer object,
but any operation requiring those bytes MUST report the missing payload.

### 3.3 Trees

A tree payload is versioned and contains one entry per repository path. Entries
MUST be sorted by canonical path and MUST NOT contain duplicates. Current
modes are:

```text
100644  regular inline or external file artifact
160000  SQLite snapshot entry
```

The entry records the path and referenced blob object ID. Tree identity is
therefore sensitive to path, mode, and blob identity.

### 3.4 Commits

A commit records:

- the root tree object ID;
- zero or more ordered parent commit IDs;
- author and committer identity/time fields;
- a Graft format/version marker;
- commit message;
- SQLite table summaries; and
- optional per-path change counts.

An initial commit has no parent, a normal commit has one parent, and a completed
three-way merge has two parents ordered as local/ours then merge target/theirs.
Commit metadata supports history summaries without requiring every tree, blob,
or SQLite page to be hydrated.

### 3.5 Tags

A lightweight tag is a ref directly naming an object. An annotated tag is a
`tag` object containing target ID, target type, tag name, tagger fields, and
message, with `refs/tags/<name>` naming that object. Tag creation MUST validate
the target. Safe deletion/overwrite MUST not silently replace an unrelated tag.

## 4. HEAD, refs, and revisions

### 4.1 Namespaces

Mutable references use these namespaces:

```text
refs/heads/<branch>
refs/remotes/<remote>/<branch>
refs/tags/<tag>
```

`HEAD` is either symbolic to a local branch or detached at a commit. An unborn
symbolic branch is valid before its first commit. Ref updates MUST be atomic at
the individual-ref boundary and SHOULD append corresponding reflog entries for
`HEAD` and the changed ref.

### 4.2 Branches and tags

Branch creation resolves its start revision. Safe branch deletion MUST reject
the checked-out branch and an unmerged branch; force deletion MAY bypass the
merge check but not corrupt `HEAD`. Rename MUST update the symbolic `HEAD` when
renaming the checked-out branch. Switching may attach `HEAD` to a branch or
detach at a resolved commit, subject to worktree/index safety checks owned by
the relevant operation.

### 4.3 Revision resolution

Revision resolution accepts full object IDs, unambiguous hexadecimal object-ID
prefixes of 4 through 63 characters, `HEAD`/`@`, local branches,
remote-tracking refs, and lightweight or annotated tags (peeled to a commit).
Parent operators select first-parent ancestry (`~n`) or a numbered parent
(`^n`); `^0` selects the resolved commit itself. Ambiguous prefixes, missing
parents, invalid names, and non-commit results where a commit is required MUST
fail explicitly.

Ancestor and merge-base queries operate on the commit graph and MUST NOT move
refs or hydrate unrelated payload data.

## 5. Index and staging

### 5.1 Stages

The index is ordered by path then stage. Stages are:

| Numeric stage | Name | Meaning |
| --- | --- | --- |
| `0` | Normal | staged result for the next commit |
| `1` | Base | common ancestor during an unresolved merge |
| `2` | Ours | local side during an unresolved merge |
| `3` | Theirs | merge-target side during an unresolved merge |

Outside an unresolved merge, a path MUST have at most one Normal entry. During
a conflict, Base/Ours/Theirs entries represent available sides; an absent side
represents deletion. A resolved path collapses to one Normal entry or a staged
deletion. `commit` MUST reject unresolved non-Normal stages.

### 5.2 Staged overlay

The Normal index is an overlay on `HEAD`: a Normal artifact/snapshot stages an
addition or replacement, while an explicit empty entry stages deletion. A
commit consumes this exact staged state. It MUST NOT recapture a newer physical
file at commit time.

Staging an ordinary file selects inline or external representation according to
Section 6. Staging a physical SQLite file captures a consistent standalone
snapshot, including committed WAL frames and excluding uncommitted changes.
The capture process MAY reuse unchanged 4 KiB snapshot content.

### 5.3 Worktree observations

The worktree observation file records paths known dirty or deleted by Graft's
adapters. It is updated atomically and is a performance/input signal, not a
substitute for validating the current file when correctness requires it.
Corrupt or stale observation data MUST be recoverable by recomputation.

## 6. Tracked files, SQLite entries, and external payloads

### 6.1 Path kinds and storage classes

Public status and inventory distinguish:

```text
path kind:  sqlite_database | text_file | binary_file
storage:    sqlite_snapshot | inline | external
```

UTF-8 text at or below the inline threshold is normally stored inline. Binary
content, text above the threshold, and paths selected by `external_paths` use
an external payload pointer. SQLite databases use a snapshot descriptor.

### 6.2 External payload store

External payload bytes are stored under `.graft/store/files` by content hash.
The pointer in the object graph remains canonical even if bytes are absent
locally. Payload status, fetch, audit/repair, and prune MUST distinguish pointer
reachability from payload availability.

Prune roots include the current index plus objects reachable from local refs,
remote-tracking refs, and tags. A conforming prune MUST NOT remove a reachable
payload. Repair MAY fetch recoverable payloads but MUST report content-hash
mismatch or unrecoverable absence.

### 6.3 Tracked roots and ignore rules

`track.default_roots` and `track.user_roots` form a normalized, de-duplicated
union. An empty union permits all otherwise visible worktree paths. A non-empty
union limits discovery/staging to those roots.

`.graftignore` and `.gitignore` contribute ignore rules. Ignore policy controls
untracked discovery and implicit staging; it MUST NOT silently untrack an
already tracked path. Inventory MUST be able to distinguish tracked,
untracked, ignored, and tracked-but-now-ignored paths.

## 7. Configuration contract

The default configuration includes repository format 2, object format
`blake3`, default branch `main`, a 1 MiB inline text threshold, no explicit
external paths or track roots, physical SQLite materialization enabled, and
the built-in merge resolver defaults defined by the merge specification.

The generic configuration command accepts only these key families:

```text
files.inline_text_threshold
files.external_paths
track.default_roots
track.user_roots
worktree.materialize_sqlite
merge.default_semantic_keys
merge.semantic_keys.<table>
merge.generated_columns.<table>
merge.internal_resolvers.<subject>
merge.schema_resolvers.<operation>
```

Values MUST be type-checked and resolver values MUST be from supported pairs.
Unsetting a scalar default restores its default; unsetting a per-table or
per-subject override removes that override. Unknown generic keys MUST fail.

Remote definitions and branch upstream configuration are persisted in the
same repository config but are managed through dedicated remote/branch
operations, not the generic configuration-key API. Credentials MUST NOT be
persisted there.

## 8. Status, inventory, and history

### 8.1 Status

Status compares `HEAD`, index, and worktree observations/content. It reports:

- staged, unstaged, and conflicted paths;
- path kind and storage class;
- Git-like two-column status codes where exposed;
- current branch/detached or unborn state;
- configured upstream and ahead/behind/diverged state; and
- active work-in-progress state, including merge state.

Status is read-only. Cached or incremental status MAY avoid recomputation, but
must invalidate on any relevant config, ref, index, ignore, tracked-path, or
worktree fingerprint change. A cache hit MUST produce the same observable
result as a full scan.

### 8.2 Inventory and ignore queries

Inventory and ignore APIs MUST be bounded and pageable where exposed through
an SDK. Explicit path queries return one result per requested canonical path;
they MUST NOT silently omit an invalid or unavailable path. Discovery results
MAY be sorted for deterministic presentation.

### 8.3 History

History summaries read commit metadata and MAY avoid tree/blob hydration.
Commit details and changed-path queries resolve trees lazily. Pagination MUST
have deterministic order and a stable cursor/offset interpretation within the
same repository state.

Changed-path classification includes additions, deletions, modifications, and
exact moves. SQLite move detection uses snapshot/path identity; ordinary file
moves require exact content identity. Similarity heuristics are not part of
version 1.0.

## 9. Restore, reset, and repository maintenance

Restore may copy a selected revision or index version into the index and/or
worktree according to explicit options. Reset modes are:

| Mode | Ref | Index | Worktree |
| --- | --- | --- | --- |
| soft | move | preserve | preserve |
| mixed | move | reset to target | preserve |
| hard | move | reset to target | project target |

Physical projection rules are owned by the materialization specification.
Reset and restore MUST reject unsafe unresolved state unless the operation
explicitly defines how that state is replaced.

Every object read MUST verify canonical bytes and the requested object ID. The
current top-level repository `audit` checks tracked artifacts and external
payload availability/integrity and can repair fetchable object/payload data
from a selected remote. The current `gc` command traces SQLite storage roots;
it is not a loose repository-object collector.

External-payload prune retains payloads referenced by the active index and by
commits reachable from local branches, remote-tracking refs, and tags. SQLite
storage GC additionally roots index snapshots, `HEAD`, branches, merge/original
heads, remote-tracking refs, and tags. These
reachability domains MUST NOT be conflated. Version 1.0 does not expose a
command that deletes unreachable loose repository objects.

## 10. Atomicity, concurrency, and failure

The current implementation has two filesystem publication classes:

- `HEAD`, ref, config, worktree-observation, and external-payload replacements
  that use the shared helper write a sibling temporary file and rename it; and
- loose object files, the index, and `MERGE_HEAD`/`ORIG_HEAD`, which currently
  use direct file writes.

Object reads verify canonical decoding and content ID, and index/merge reads
parse their complete format, so a torn/corrupt direct write is detected rather
than accepted. However, version 1.0 does **not** claim crash-atomic loose-object,
index, or merge-record persistence. The temp-and-rename helper also does not
currently specify file/directory `fsync` durability. Reflog append occurs after
the associated ref replacement and is not one transaction with it.

Higher-level commit/publication code writes required immutable content before
moving a ref. Operations carrying an expected head/token MUST reject mismatch
as stale. Repository mutations coordinate through the command-service/storage
lock boundary; one retained SDK session additionally serializes its calls.
Independent direct core users or processes must still honor those coordination
and expected-state rules.

Multi-file operations are not one filesystem transaction. Durable merge
records, temporary paths, validation, and recovery checks make common
interruptions detectable, but a power loss during a direct metadata write may
require repair from reachable objects/refs or a remote. No recovery path may
invent missing content or silently discard both sides of a conflict.

## 11. Conformance requirements

A `GRAFT-Core-1.0` repository implementation MUST demonstrate:

1. deterministic object IDs for canonical blob/tree/commit/tag vectors;
2. rejection of malformed paths, envelopes, lengths, IDs, trees, and snapshot
   descriptors;
3. safe repository discovery and unsupported-format rejection;
4. correct unborn, attached, and detached `HEAD` behavior;
5. temp-and-rename ref/HEAD replacement, operation-level expected-state
   rejection, and separately appended reflog behavior;
6. exact index stage transitions and commit rejection with conflicts;
7. staging from consistent SQLite and file-artifact sources;
8. ignore and track-root behavior without implicit untracking;
9. equivalent full and cached/incremental status results; and
10. object-read verification plus reachability-safe payload and storage
    maintenance.

Current implementation evidence is concentrated in `crates/graft/src/repo/`,
repository tests in `crates/graft/src/repo/tests.rs`, SQLite staging and command
service tests in `crates/graft-sqlite/src/`, and adapter integration tests.

## 12. Compatibility notes and known limits

- Readers still accept `file-blob-v1`; writers emit `file-blob-v2`.
- Repository format 2 and object envelope 1 are current persisted contracts;
  there is no promise that private Fjall keys are portable APIs.
- Move detection is exact, not similarity-based.
- The status cache is reconstructable and never canonical repository state.
- The generic config API intentionally does not expose arbitrary TOML editing.
- Version 1.0 has payload prune and SQLite storage GC, but no public loose-object
  GC command.
- Loose object, index, and merge-record writes are validated on read but are
  not yet crash-atomic/fsync-durable; reflog and ref replacement are separate.
- Formal machine-readable conformance/capability records are not yet emitted.
