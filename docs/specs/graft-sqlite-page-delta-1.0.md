# Graft SQLite Page Delta 1.0

Status: Draft implementation-aligned normative specification
Canonical language: English
Conformance profile: `GRAFT-Delta-1.0`

## 1. Scope

This specification defines the portable `GRAFTD01` delta between two exact,
consistent SQLite images. It owns the binary format, validation rules, and the
repository-independent create, inspect, and apply operations.

The format is a fixed-page transport optimization. It is not a logical SQLite
changeset, a generic binary-diff format, a merge format, or a replacement for
Graft repository history.

## 2. Terms

- **Base**: the exact SQLite image required to apply a delta.
- **Target**: the exact SQLite image produced after successful application.
- **Page**: one 4 KiB transport block, numbered from one.
- **Changed page**: a target page whose exact bytes differ from the
  corresponding base page, or a target page beyond the end of the base.
- **Consistent image**: a standalone main-database image that includes the
  committed state visible through SQLite, including committed WAL content.

## 3. Format identifier and media type

The format name is `graft-sqlite-page-delta-v1`. Its eight-byte magic is the
ASCII sequence `GRAFTD01`. A network adapter MAY use the media type
`application/vnd.eidos.sqlite-page-delta`; the media type does not change the
format semantics.

## 4. Integer and digest encoding

All integer fields are unsigned and little-endian. SHA-256 fields contain the
32 raw digest bytes, not hexadecimal text. Byte offsets are zero-based.

## 5. Fixed header

Version 1 has a 104-byte header:

| Offset | Bytes | Field | Version 1 value or meaning |
| ---: | ---: | --- | --- |
| 0 | 8 | magic | `GRAFTD01` |
| 8 | 4 | header bytes | `104` |
| 12 | 4 | flags | `0` |
| 16 | 4 | page bytes | `4096` |
| 20 | 4 | changed page count | number of entries |
| 24 | 8 | base bytes | exact base image length |
| 32 | 8 | target bytes | exact target image length |
| 40 | 32 | base SHA-256 | digest of exact base bytes |
| 72 | 32 | target SHA-256 | digest of exact target bytes |

Base and target byte lengths MUST be non-zero multiples of 4096. The changed-page count
MUST NOT exceed the target page count. A version 1 reader MUST reject a header
length other than 104, non-zero flags, or a page size other than 4096.

## 6. Page entries

The header is followed by exactly `changed page count` entries. Every entry is:

| Bytes | Field |
| ---: | --- |
| 4 | one-based target page number |
| 4096 | exact target page bytes |

Page numbers MUST be strictly increasing, unique, non-zero, and no greater than
`target bytes / 4096`. Every target page beyond the end of the base MUST have an
entry. Target truncation is represented only by `target bytes`; no tombstone
entry is written for removed trailing pages.

The exact delta length is:

```text
104 + changed_page_count * (4 + 4096)
```

A reader MUST reject a different physical length.

## 7. Create operation

A conforming creator MUST:

1. obtain consistent standalone images of both SQLite inputs;
2. compute SHA-256 over every exact image byte;
3. compare target pages against base pages using exact bytes;
4. write entries in increasing page order;
5. create, rather than overwrite, the output path;
6. remove a partial output when creation fails.

The creator reports whether the resulting delta is smaller than the target,
but MUST NOT silently substitute a full target file when it is not beneficial.

## 8. Inspect operation

Inspection MUST validate the fixed header, file length, and all page-number
constraints without requiring the base. It returns the embedded sizes,
digests, page counts, and whether the delta is smaller than the target.
Inspection does not prove that the named base exists or that the target can be
materialized.

## 9. Apply operation

A conforming applier MUST:

1. obtain a consistent standalone base image;
2. require its exact byte length and SHA-256 to match the header;
3. stream base pages and replacement entries into a create-new output;
4. truncate or extend according to the exact target byte length;
5. require every appended page to be present in the delta;
6. verify the complete output SHA-256 against the target digest;
7. remove a partial output on any failure.

The applier MUST NOT weaken a base mismatch into a best-effort application and
MUST NOT overwrite an existing output.

## 10. CLI mapping

The native CLI exposes the repository-independent commands:

```text
graft delta create --base BASE --target TARGET --output DELTA [--json]
graft delta apply --base BASE --delta DELTA --output TARGET [--json]
graft delta inspect DELTA [--json]
```

These commands MUST NOT require or mutate a `.graft` repository and MUST reject
the global `--db` option. JSON output MUST remain the only stdout content when
`--json` is selected and MUST identify the operation and paths plus the format
metadata.

## 11. SDK capture mapping

An SDK publication capture MAY create `GRAFTD01` directly from immutable Graft
snapshots. When it does, its opaque base token MUST retain the exact base digest
used in the next delta, and the returned result MUST expose the complete target
SHA-256. SDK delta generation remains read-only with respect to repository
history, refs, index, and worktree identity.

## 12. Resource bounds and security

Implementations SHOULD process SQLite images page-by-page. Network adapters MAY
apply a stricter delta-size limit and buffer only within that bound. Inputs are
untrusted: all arithmetic MUST be checked, page numbers MUST be validated
before allocation or seeking, and the embedded digests MUST be matched to any
external immutable-object identity used by the adapter.

## 13. Compatibility

The 104-byte layout is the complete version 1 baseline. A future compatible
extension requires a new declared header size and flags understood by both
parties; a reader conforming only to version 1 rejects it. A semantic or entry
encoding change requires a new magic/version rather than ambiguous guessing.
