# Graft Diff 1.0

Status: Draft implementation-aligned normative baseline
Version: 1.0
Published: 2026-08-11
Canonical language: English

## Abstract

This specification defines read-only comparison of repository paths, ordinary
file content, SQLite rows and schemas, opaque SQLite surfaces, historical
changes, and bounded/paginated result APIs. It also defines the capability and
limitation disclosures required when a logical SQLite diff cannot be produced
directly.

Diff is observation only. It never stages a resolution, advances a ref,
changes durable merge state, or writes an application worktree file.

## 1. Scope and inputs

A diff compares two **sides**. A side may be a commit/revision tree, the index,
the current worktree, a merge version (`base`, `ours`, `theirs`, `result`), or
an explicitly supplied snapshot/artifact. The caller MUST identify sides
unambiguously; omitted sides use the operation's documented default.

The current repository/SDK comparison modes are:

| Options | Comparison |
| --- | --- |
| default | `HEAD` versus current worktree, with staged/unstaged classification in the general result |
| `staged` | `HEAD` versus Normal index result |
| `root = R` | empty tree versus revision `R` |
| `from = A` | revision `A` versus current worktree |
| `from = A, to = B` | revision `A` versus revision `B` |
| merge `version` | exact active-merge side named by the request |

`root` is mutually exclusive with `from`/`to`; `to` requires `from`. Typed
SQLite `stagedFallback` may use the staged comparison only when the requested
path has no worktree change. It MUST report which comparison supplied the
result. Explicit path/table filters narrow work; they do not alter side
identity.

The CLI `--no-index` mode compares exactly two ordinary physical SQLite files
without repository discovery. Its sides are the consistent images captured
from the explicit `from` and `to` paths. It accepts `--rows` and `--json`, but
not repository selectors (`staged`, `root`, `kind`, `content`) or `--db`.
Non-SQLite inputs are rejected rather than treated as generic binary files.
JSON identifies both sides by path and page count, reports `changed`, `kind`,
and whether rows were requested, and includes `row_diff` only when logical
expansion was requested for a changed pair.

Repository path and artifact identity are defined by [Graft Repository
1.0](./graft-repository-1.0.md). Storage snapshots and hydration are defined by
[Graft Storage and Snapshots 1.0](./graft-storage-snapshots-1.0.md). Merge
versions are defined by [Graft Merge 1.0](./graft-merge-1.0.md).

## 2. Read-only invariant

Every diff, history-summary, commit-detail, changed-path, content-read, and row
inspection operation MUST be observational with respect to repository state.
It MAY hydrate immutable objects/pages or create temporary compatibility files,
but MUST NOT:

- move `HEAD` or another ref;
- change the index or durable merge state;
- create, replace, or remove a tracked worktree path;
- resolve a conflict; or
- publish remote mutable state.

Hydration and temporary `materialized_compat` databases are explicitly not
physical worktree materialization.

## 3. Path-level diff

### 3.1 Change classes

For each canonical repository path, a path diff reports one of:

```text
added | deleted | modified | unchanged
```

APIs that omit unchanged paths MUST say so. A path result includes available
kind/storage metadata and side object/snapshot identities. Addition/deletion
has only one content-bearing side. A changed kind or storage representation is
at least `modified`, even if a higher layer later determines equivalent bytes.

### 3.2 Moves

Changed-path history MAY pair one deletion and one addition as an exact move.
For SQLite entries, pairing uses exact snapshot identity. For ordinary file
artifacts, pairing uses exact content identity. Version 1.0 does not define a
similarity score, copy detection, or ambiguous many-to-many rename heuristic.

### 3.3 Ordering and pagination

Path results MUST use deterministic canonical-path order unless a specific
history API documents commit order. Pagination MUST not duplicate or omit a
path within a stable comparison. SDK path pages have a current maximum page
size of 100 and a maximum request budget of 10,000 paths.

## 4. Ordinary file content

### 4.1 Text

An inline or hydrated external file whose bytes are valid UTF-8 MAY be exposed
as text. Text diff presentation (unified hunks, side-by-side rows, context
folding) is an adapter/UI concern; the canonical observation is the exact bytes
or decoded text for each side.

No newline normalization, character replacement, or encoding conversion may be
used to claim byte equality. A host MAY render newline markers or replacement
glyphs but MUST retain an exact-content retrieval path.

### 4.2 Binary and external content

Binary paths report identities, sizes, availability, and changed status.
Adapters MAY expose bounded bytes for inspection. The current SDK content-read
limit is 8 MiB. A missing external payload MUST be distinguished from an empty
file and SHOULD include the payload identity needed for fetch/repair.

Content reads MUST validate requested path, side, size bound, and payload hash.
They MUST NOT silently truncate without returning truncation metadata or an
explicit size-limit state/error.

The retained SDK's `readPathContent` reads one path from an explicitly supplied
immutable revision. Active merge versions use `readMergeVersion`; current
worktree file reading remains a host/filesystem concern rather than being
silently substituted for an immutable revision.

## 5. SQLite logical diff model

### 5.1 Result domains

A SQLite logical diff contains four independent domains:

1. table/row changes;
2. schema changes;
3. opaque changes that cannot be safely represented as ordinary rows/schema;
4. capability, limitation, and response-scope metadata.

An empty row list does not imply an unchanged SQLite file. Schema or opaque
changes may exist, and raw pages may differ with no supported logical change.

### 5.2 Row identity

A row is identified by either:

- signed 64-bit `rowid` for a rowid table; or
- the declared ordered primary-key tuple for a `WITHOUT ROWID` table.

Primary-key parts are typed values:

```text
null | integer | real | text | blob
```

Integer and blob/text identity is exact. Real identity uses normalized floating
bits; positive and negative zero compare as the same key. Composite key order
is schema order and MUST be preserved.

### 5.3 Row changes

Row changes are `insert`, `delete`, or `update`. An update includes the row
identity plus before/after values sufficient for inspection. Column order is
the declared table order. Generated columns MAY be reported as observations
but are marked so merge/apply code can avoid writing them.

Results distinguish SQL `NULL` from absent metadata through field presence and
JSON `null`. The current JSON row-value compatibility shape uses JSON numbers,
strings, and null; a BLOB in a row's `values`/`old_values` is rendered as a
lowercase hexadecimal **bare string**, which is ambiguous with SQLite TEXT.
Primary-key BLOBs use the unambiguous `{ "$blob": "<hex>" }` marker accepted by
row-resolution input. Clients MUST use schema/type context and MUST NOT assume
that arbitrary row-value JSON is a lossless typed round trip. Replacing the
bare-string compatibility shape requires a versioned result-contract change.

### 5.4 Schema changes

Schema entries are compared as logical SQLite schema records and classified as
`added`, `deleted`, or `modified`. The result identifies schema object type and
name and includes available before/after SQL. A changed table definition MUST
not be reduced to row-only output.

### 5.5 Opaque changes

Opaque entries are `added`, `deleted`, or `modified` and include a reason and,
when known, owner table/object. Opaque surfaces include virtual-table state,
FTS shadow structures, SQLite internal tables, and index B-tree changes not
represented as ordinary application rows. They MUST be surfaced rather than
silently ignored.

## 6. Capabilities, limitations, and logical status

The current engine advertises these logical capabilities:

```text
rowid_table_rows
primary_key_table_rows
schema_entries
opaque_table_detection
semantic_insert_keys
```

It can disclose these limitations:

```text
virtual_table
fts_shadow_table
sqlite_internal_table
index_btree
utf16_text_encoding
generated_columns
```

The top-level logical status is one of:

| Status | Meaning |
| --- | --- |
| `logical_changes` | at least one supported row/schema/opaque observation exists |
| `unsupported_logical_surface` | a changed surface cannot be completely interpreted |
| `file_changed_no_supported_logical_changes` | bytes/pages changed but no supported logical change was produced |

Repository-level APIs MAY additionally return `row_diff_unavailable` when no
two SQLite snapshots exist, such as path addition/deletion or missing data.
Limitations are evidence about completeness, not errors to suppress.

## 7. Execution scopes

Each bounded SQLite response reports the execution scope used:

| Scope | Meaning |
| --- | --- |
| `streaming_rowid` | direct 4 KiB B-tree/page traversal for rowid rows |
| `streaming_primary_key` | direct traversal keyed by declared primary key |
| `streaming_btree` | direct schema/opaque B-tree traversal |
| `materialized_compat` | temporary read-only SQLite databases used for compatibility |
| `unavailable` | logical execution could not be provided |

Direct streaming compares changed B-tree pages/rows and SHOULD avoid loading
the entire database. `WITHOUT ROWID` tables use typed declared keys when the
layout is directly supported and fall back to `materialized_compat` otherwise.

`materialized_compat` databases MUST be created in isolated temporary paths,
opened read-only for comparison, and use defensive settings including
`trusted_schema=OFF`. They MUST be cleaned up on success and failure. They are
not worktree files and MUST NOT alter repository state.

`--no-index` captures each physical side through SQLite's consistent backup
path, including committed WAL frames. Its file-level equality uses the same
4 KiB snapshot comparison as repository worktree diff and ignores only the
volatile SQLite page-1 cache-invalidation counters and last-writer library
version. Physical changes with no supported logical row/schema result remain
changed and use `file_changed_no_supported_logical_changes` when row expansion
was requested.

## 8. Bounded row APIs

A row-diff request chooses either a summary across tables or rows for one table.
Current limits are 1 through 1000 rows/items per page. Offset/cursor values MUST
be validated and deterministic for a stable pair of snapshots.

Responses include enough metadata to continue pagination, determine whether
more results exist, identify table and row identity mode, and understand the
execution scope and limitations. Cancellation checkpoints SHOULD occur inside
long B-tree/table loops, not only before and after the complete request.

A summary count MAY be cheaper than materializing every row result, but it MUST
not claim an exact count if execution stopped at a configured bound. Exact,
bounded, and lower-bound counts must be distinguishable.

## 9. Historical and merge-version inspection

History summary APIs MAY use commit metadata without hydrating SQLite pages.
Requesting row details, path content, or a historical schema MAY hydrate only
the required object, payload, and snapshot frames.

During a merge, the versions are:

```text
base    common-ancestor path version, if present
ours    local pre-merge path version, if present
theirs  target path version, if present
result  current staged/candidate resolution, if present
```

Reading any version is observational. An absent version represents deletion or
no current candidate and MUST NOT be synthesized from another side. Updating
`result` belongs to the merge specification.

## 10. Failure and stale-state behavior

Diff operations MUST report malformed SQLite pages/schema, missing objects or
payloads, hash mismatch, unsupported encoding/surface, invalid cursors, and
cancellation with distinguishable errors or limitation records.

A retained SDK MAY fingerprint repository state before and after a multi-step
read. If concurrent changes prevent one coherent observation after the allowed
retries, it MUST return a stale-state error rather than combining unrelated
versions. The current SDK performs up to three stability attempts for relevant
path races.

## 11. Conformance requirements

A conforming diff implementation MUST test:

1. deterministic path ordering and bounded pagination;
2. exact add/delete/modify and exact-move classification;
3. UTF-8, binary, missing external payload, and content-size behavior;
4. rowid insert/delete/update with typed values;
5. composite primary-key and `WITHOUT ROWID` identity;
6. schema add/delete/modify reporting;
7. opaque virtual/FTS/internal/index changes;
8. logical status and limitation completeness;
9. direct streaming versus `materialized_compat` result equivalence where both
   are supported;
10. read-only behavior for historical and merge-version inspection; and
11. cancellation and stale-state handling without partial repository writes.

Current evidence lives in `crates/graft/src/repo/diff.rs`, repository history
tests, `crates/graft-sqlite/src/row_level_diff.rs`, bounded row-diff tests, and
Rust/Node SDK result-contract tests.

## 12. Known limits

- Direct logical parsing is optimized for 4096-byte SQLite snapshots.
- UTF-16 databases and several virtual/internal surfaces are disclosed as
  limited or opaque rather than silently interpreted.
- `materialized_compat` can be slower and use temporary disk space.
- Non-key BLOB row values use a legacy bare hexadecimal JSON string and are
  ambiguous with TEXT without schema context; primary-key BLOBs are tagged.
- Version 1.0 has exact move detection only.
- Diff presentation, hunk algorithms, and UI folding are not canonical API.
