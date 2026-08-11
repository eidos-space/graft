# Graft Storage and Snapshots 1.0

Status: Draft implementation-aligned normative baseline
Version: 1.0
Published: 2026-08-11
Canonical language: English

## Abstract

This specification defines Graft's immutable 4 KiB page model, logs and logical
sequence numbers, volumes, storage commits, snapshots, readers and writers,
hydration, snapshot publication, and storage garbage collection. This storage
layer is below repository objects: a repository SQLite blob names and validates
a storage snapshot, while refs and commits remain repository concerns.

## 1. Scope and boundaries

This document owns the meaning of page, log, LSN, volume, storage commit,
snapshot range, hydration, and storage reachability. It does not define:

- repository object envelopes or path identity;
- row/schema interpretation of SQLite bytes;
- merge topology or conflict policy;
- remote branch/ref publication; or
- writing an ordinary SQLite file into the worktree.

Those contracts are defined by the repository, diff, merge, remote-sync, and
worktree-materialization specifications respectively.

## 2. Fundamental types and invariants

### 2.1 Page geometry

The Graft storage page size is exactly 4096 bytes. `PageIdx` is a non-zero,
one-based page position; the first page is `1`. `PageCount` is the logical
number of pages in a snapshot or volume state. Operations reading or writing a
page MUST use exactly one 4096-byte page. Byte offsets are zero-based and MUST
be converted to one-based page indices with overflow and bounds validation.

SQLite page numbers correspond directly to Graft's one-based page indices. A
non-4096-byte SQLite file may be handled by a higher-level
compatibility path, but it is not directly page-compatible with this store.

### 2.2 Identifiers

`LogId`, `VolumeId`, and segment identifiers are opaque stable IDs. Hosts MUST
NOT derive authorization, ordering, or repository path from their textual
form. A snapshot may contain ranges from multiple logs.

### 2.3 Logical sequence numbers

An LSN is a non-zero monotonically increasing logical sequence number scoped
to one `LogId`. It is neither globally unique nor a content hash. The pair
`(LogId, LSN)` identifies one storage commit. Comparisons between LSNs from
different logs have no ordering meaning unless a snapshot explicitly orders
their ranges.

## 3. Storage commits and segments

A storage commit records at least:

- `LogId` and LSN;
- resulting logical page count;
- page-set/segment index describing changed pages;
- frame ranges locating immutable page data; and
- when present, commit hash, checkpoints, timestamp, and message metadata.

The commit represents the complete logical state obtained by applying its
changed pages and page-count transition to prior state in the same log. It is
not a repository commit and MUST NOT be displayed as one without qualification.

A segment contains immutable page frames. The segment index maps changed page
indices to frame ranges. Storage reads MUST verify available commit/segment
metadata and MUST report corruption or missing data rather than substituting
zeroes for a page that should exist. Immutable segment data MAY be deduplicated,
packed, cached, or remotely fetched without changing snapshot semantics.

## 4. Snapshots

### 4.1 Snapshot structure

A snapshot consists of:

```text
page_count
ordered ranges: [ (log_id, start_lsn, end_lsn), ... ]
```

Each range is inclusive and MUST have non-zero `start_lsn <= end_lsn`. Range
order is significant and highest-precedence first: the first range overlays
later fallback ranges when resolving page state. The snapshot head, when
present, is the end of its first range. An empty snapshot has page count zero
and no ranges.

A repository `sqlite-snapshot-v1` blob additionally records the expected
storage commit hash for every represented LSN. Consumers MUST validate those
hashes before trusting the snapshot as canonical repository content.

### 4.2 Read semantics

To resolve a page, a reader searches snapshot state from newest applicable
commit toward older state and returns the newest frame for that page. A page
index at or beyond `page_count` is outside the logical file. Missing local
commit or frame data MAY trigger lazy remote fetch; failure to fetch required
data MUST be reported.

Opening a reader is logically read-only. It MUST NOT advance a volume, append a
storage commit, move a repository ref, or materialize a worktree file.

### 4.3 Equality and checksum

Snapshot equality is defined by canonical snapshot identity/descriptor, not by
whether two caches happen to contain the same local frames. APIs MAY compute
page checksums or a whole-snapshot checksum for audit and transfer planning.
Checksum verification MUST cover the bytes and range it claims to verify.

## 5. Volumes and log relationships

A volume is a mutable handle to a current snapshot lineage. It records a
`VolumeId`, a local log, a remote log, a synchronization point, and optionally
a pending storage commit/publication record.

Volume status compares local and remote lineage relative to the sync point and
reports an equivalent of up-to-date, ahead, behind, or diverged. This is a
storage-lineage status, separate from repository branch ahead/behind state.

Creating a volume establishes a new writable lineage. Opening an existing
volume MUST preserve its current state. Checking out a historical log reference
into a volume MUST create a new volume/lineage and MUST NOT mutate the source
history. Resetting a volume changes its current storage position according to
the explicit operation and must preserve recoverability rules for pending work.

## 6. Readers, writers, and page-count transitions

### 6.1 Reader

A `VolumeReader` exposes a stable snapshot. Concurrent later commits MUST NOT
change the bytes observed through that reader. Implementations MAY lazily fetch
frames while preserving the same logical snapshot.

### 6.2 Writer

A `VolumeWriter` starts from a base snapshot, overlays dirty pages, and tracks a
logical page count. Reads through the writer observe dirty pages first and base
snapshot pages otherwise. Commit appends an immutable storage commit and
returns a reader for the resulting snapshot.

A writer MUST reject invalid page sizes or indices and MUST serialize
publication for its target lineage. Cancellation MAY stop before publication;
after a remote/publication boundary the result may require recovery as defined
in Section 9.

### 6.3 Soft truncate

Truncate changes the logical page count. Current storage uses soft-truncate
semantics: shrinking hides pages beyond the new count but does not guarantee
their frames are erased; re-expanding before overwriting MAY reveal prior page
bytes. Callers requiring zero-filled new SQLite pages MUST write them. This
behavior is part of 1.0 and MUST NOT be mistaken for secure erase.

## 7. Runtime and local storage

The runtime binds asynchronous execution, local storage, and a remote backend.
Clones of one runtime share the same underlying instance and coordination.
The current native local store uses Fjall, but its private key encoding is not
a portable interface.

Runtime operations include tag and volume management, volume reader/writer,
pull/push/status/snapshot, log fetch, commit-hash lookup, page/checksum queries,
snapshot hydration/publication, volume checkout/diff/reset, and storage GC.
Adapters MAY expose only a subset but MUST preserve semantics of exposed calls.

Local caches MAY omit remotely available immutable commits/segments. Cache
absence is not snapshot absence. Conversely, a locally present unreferenced
segment is not proof that any repository commit reaches it.

## 8. Hydration and snapshot publication

### 8.1 Hydration

Hydration ensures required storage commits and page frames for a snapshot are
available locally. It MAY be exact, demand-driven, or cached. An exact hydration
MUST fetch every commit/frame needed to read every logical page of the target
snapshot and MUST validate expected commit hashes when supplied.

Hydration is not physical worktree materialization. It MUST NOT create or
replace an application SQLite file, move `HEAD`, or alter the index. Cached
hydration results are reconstructable and MAY be evicted.

### 8.2 Missing-page planning

An implementation MAY enumerate missing pages/frames and transfer only those
not available locally. Such optimization MUST preserve final snapshot bytes
and validation. A partially hydrated snapshot MUST not be reported as fully
available for an operation that requires all pages.

### 8.3 Snapshot push

Snapshot push prepares and publishes immutable storage commits and segments
needed by a target remote. Immutable data MUST be published before mutable
metadata that makes it reachable. Re-uploading the same immutable key MUST be
idempotent or content-equal.

Storage snapshot publication alone does not update a repository branch. The
remote-sync layer coordinates snapshot data, repository objects/payloads, and
the final branch compare-and-swap.

## 9. Failure, cancellation, and pending publication

Local storage commit publication MUST be atomic from a reader's perspective:
readers see the old or new committed state, not a half-indexed commit.

Volume push maintains a pending-commit/publication record when a remote outcome
cannot safely be inferred. Cancellation is guaranteed only at safe boundaries.
After sending a publication request, timeout or transport loss can mean:

- publication was rejected and state is known unchanged;
- publication may have succeeded but acknowledgement was lost; or
- outcome is otherwise unknown.

The implementation MUST preserve enough pending state to inspect/reconcile the
remote and MUST NOT blindly republish a different successor. Adapters map these
states to distinct publication-unconfirmed/outcome-unknown errors.

Corruption, hash mismatch, non-contiguous required history, and missing
unrecoverable frames MUST fail explicitly. Recovery MUST NOT fabricate page
content or silently move the synchronization point.

## 10. Storage garbage collection

Storage GC traces snapshots/volumes supplied as roots to reachable logs,
commits, segments, and frames. Repository refs, index state, merge state, and
payload pointers are translated into storage roots by the repository layer.

GC MUST retain all data needed by every root and any pending publication or
recovery record. It MAY remove unreferenced immutable data after an appropriate
grace/coordination boundary. Object GC and external-payload prune are separate
domains; running one MUST NOT imply the others ran.

## 11. Conformance requirements

A storage implementation claiming `GRAFT-Core-1.0` MUST test:

1. exact 4096-byte page validation and index/count bounds;
2. non-zero per-log LSN ordering and range validation;
3. deterministic snapshot overlay across one and multiple logs;
4. stable readers across later writer commits;
5. writer read-your-writes and page-count behavior;
6. documented soft-truncate/re-expansion behavior;
7. lazy and exact hydration with hash/missing-data failures;
8. idempotent immutable publication and pending-outcome recovery;
9. historical volume checkout without source mutation; and
10. reachability-safe storage GC.

Current evidence lives in `crates/graft/src/rt/`, `crates/graft/src/local/`,
snapshot/log/volume model tests, storage action tests, and repository SQLite
snapshot integration tests.

## 12. Compatibility notes and known limits

- Graft's page size is fixed at 4096 bytes in version 1.0.
- LSNs are logical per-log counters, not hashes or global clocks.
- Soft truncate does not securely erase hidden frames.
- Fjall is the current local engine, not a standardized interchange format.
- Hydration may be lazy; callers requiring complete local availability must
  request an operation with that postcondition.
- Repository-branch publication atomicity is owned by Remote Sync, not by a
  standalone storage push.
