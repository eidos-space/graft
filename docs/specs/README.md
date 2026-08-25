# Graft Specifications

Status: Specification suite index
Suite version: 1.0
Canonical language: English

This directory is the implementation-aligned contract for Graft. It records
the observable behavior shared by the repository core, page store, SQLite
integration, remote protocol, command-line interface, SDKs, and browser/WASM
host. It follows the ownership discipline used by the Eidos Runtime and
Adapter specifications: each behavior has one normative owner and adapters
map that behavior without redefining it.

The suite specifies stable contracts and current compatibility boundaries. It
does not freeze private Rust types, storage-engine key layouts, UI appearance,
or implementation techniques that have no observable effect.

## Documents

| Area | Canonical specification | Chinese reference | Primary owner |
| --- | --- | --- | --- |
| Repository and object model | [Graft Repository 1.0](./graft-repository-1.0.md) | [中文](./graft-repository-1.0.zh.md) | `graft::repo` |
| Page storage and snapshots | [Graft Storage and Snapshots 1.0](./graft-storage-snapshots-1.0.md) | [中文](./graft-storage-snapshots-1.0.zh.md) | `graft` runtime/storage |
| Portable SQLite page deltas | [Graft SQLite Page Delta 1.0](./graft-sqlite-page-delta-1.0.md) | [中文](./graft-sqlite-page-delta-1.0.zh.md) | `graft-sqlite` |
| Diff and historical inspection | [Graft Diff 1.0](./graft-diff-1.0.md) | [中文](./graft-diff-1.0.zh.md) | repository diff + `graft-sqlite` row diff |
| Merge and conflict recovery | [Graft Merge 1.0](./graft-merge-1.0.md) | [中文](./graft-merge-1.0.zh.md) | repository merge + SQLite row merge |
| Remotes and synchronization | [Graft Remote Sync 1.0](./graft-remote-sync-1.0.md) | [中文](./graft-remote-sync-1.0.zh.md) | repository sync + remote protocol |
| Runtime, CLI, and SDK adapters | [Graft Runtime and Adapters 1.0](./graft-runtime-adapters-1.0.md) | [中文](./graft-runtime-adapters-1.0.zh.md) | command service + SDK adapters |
| Physical worktree projection | [Graft Worktree Materialization 1.0](./graft-worktree-materialization-1.0.md) | [中文](./graft-worktree-materialization-1.0.zh.md) | SQLite worktree adapter |

## Reading order

The repository specification defines identity, objects, refs, and index state.
The storage specification defines the page/log substrate referenced by SQLite
snapshot objects. Diff and merge consume both models. Remote sync transports
their immutable and mutable state. Runtime/adapters expose those operations to
hosts. Worktree materialization creates or replaces ordinary SQLite files in a
physical worktree.

```text
Repository objects / refs / index
          |                 |
          v                 v
Storage snapshots         Diff + Merge
          \                 /
           v               v
             Remote sync
                  |
          Runtime / adapters
             /           \
      SQLite VFS     Physical worktree
```

## Specification status and language

The English documents are normative. Chinese documents are informative,
section-aligned references and MUST NOT change the English meaning. Every 1.0
document is a **draft implementation-aligned normative baseline**: requirements
are backed by current source and tests, while known gaps and compatibility
limits are stated explicitly instead of being promoted into promises.

The key words **MUST**, **MUST NOT**, **REQUIRED**, **SHALL**, **SHALL NOT**,
**SHOULD**, **SHOULD NOT**, **RECOMMENDED**, **NOT RECOMMENDED**, **MAY**, and
**OPTIONAL** are interpreted as described by BCP 14 only when they appear in
all capitals.

## Ownership and single source of truth

| Observable concern | Normative owner |
| --- | --- |
| Repository discovery, normalized paths, objects, commits, refs, index, status, history | Repository |
| 4 KiB pages, logs, LSNs, volumes, snapshots, hydration, storage GC | Storage and Snapshots |
| Repository-independent SQLite delta format, creation, inspection, and application | SQLite Page Delta |
| Path/content/row/schema/opaque comparison and bounded inspection | Diff |
| Topology, merge state, conflicts, resolutions, continue/abort | Merge |
| Remote URI/backend behavior, wire protocol, fetch/push/pull/clone and publication | Remote Sync |
| Operation names, session lifecycle, serialization, cancellation, errors and host mapping | Runtime and Adapters |
| Live SQLite VFS, extension registration, lock state, production PRAGMAs | SQLite Integration |
| Ordinary SQLite files, WAL/sidecars, replacement locks, recovery | Worktree Materialization |
| Playground layout and visual interaction | UI implementation; outside this suite |

An upper-layer specification MAY summarize a lower-layer rule but MUST link to
its owner and MUST NOT redefine it. Source comments, READMEs, guides, and old
design records are implementation evidence; they do not amend this suite.
When an older document conflicts with current code, tests, and this suite, the
versioned specification controls the intended behavior and the discrepancy is
a documentation or implementation conformance issue.

## Cross-cutting invariants

All conforming profiles preserve these rules:

1. Repository paths are normalized UTF-8 relative paths and never address
   `.graft` internals.
2. Object and snapshot identity is content-derived; mutable refs never alter
   immutable object bytes.
3. Staging, committing, hydration, temporary diff databases, export, live VFS
   access, and physical worktree materialization are distinct operations.
4. Read-only planning and inspection do not silently move refs, replace the
   worktree, or resolve conflicts.
5. Destructive or state-changing operations detect stale state and expose a
   retryable error rather than silently overwriting a concurrent result.
6. Remote credentials remain explicit adapter inputs and are not persisted in
   repository config, URLs, caches, or result payloads.
7. Browser/WASM hosts disclose unavailable native capabilities and do not use
   mocks as evidence of core conformance.

## Conformance profiles

Each document defines focused requirements. The suite-wide profile names are:

```text
GRAFT-Core-1.0       repository, object, ref, index, storage, diff, and merge
GRAFT-Remote-1.0     remote protocol and synchronization
GRAFT-CLI-1.0        command-line mapping over the command service
GRAFT-SDK-1.0        retained Rust and Node repository sessions
GRAFT-Browser-1.0    WASM/OPFS host composition and capability disclosure
GRAFT-VFS-1.0        live SQLite VFS and extension surface
GRAFT-Worktree-1.0   ordinary-file materialization and recovery
GRAFT-Delta-1.0      portable SQLite page-delta creation and application
```

An implementation MUST claim profiles separately. A profile claim identifies
behavior, not a particular crate or language binding. The current repository
does not yet emit a machine-readable capability record, so these labels are
conformance targets rather than release claims.

## Implementation evidence map

| Specification | Primary source | Executable evidence |
| --- | --- | --- |
| Repository | `crates/graft/src/repo.rs`, `crates/graft/src/repo/` | repository/object/ref/index/history/inventory tests |
| Storage and Snapshots | `crates/graft/src/core/`, `snapshot.rs`, `volume.rs`, `rt/`, `local/` | runtime, storage action, snapshot hydration and GC tests |
| SQLite Page Delta | `crates/graft-sqlite/src/page_delta.rs` | create/inspect/apply round trips, digest rejection, CLI and SDK capture tests |
| Diff | repository diff/history plus `crates/graft-sqlite/src/row_level_diff.rs` | rowid/PK/schema/opaque/bounded diff tests and SDK contracts |
| Merge | `crates/graft/src/repo/merge.rs`, `graft-sqlite` row merge/output | core topology, SQLite conflict/resolution, reopen and browser fixture tests |
| Remote Sync | repository sync, remote/runtime actions, `packages/graft-remote` | Rust remote tests plus framework/Hono/Cloudflare protocol tests |
| Runtime and Adapters | command service, Rust SDK, Node addon/package, `web-demo` worker | lifecycle/cache/cancel/error/contract tests and Playwright WASM tests |
| Materialization | `graft-sqlite` checkout/snapshot/merge worktree paths | WAL/replacement/recovery/operation-gate integration tests |

The tables identify evidence, not ownership by filename. A behavior spanning
several crates still has the single normative owner listed earlier.

## Current compatibility and debt register

These items are deliberate limits of the 1.0 baseline, not stronger guarantees
that an adapter may silently fill in:

| Current limit or documentation drift | Normative owner |
| --- | --- |
| Loose-object, index, and merge-record direct writes are validated on read but are not crash-atomic or fsync-durable | [Repository](./graft-repository-1.0.md#10-atomicity-concurrency-and-failure) |
| Generic non-key BLOB row values use legacy bare hexadecimal JSON strings and need schema context to distinguish them from TEXT | [Diff](./graft-diff-1.0.md#12-known-limits) |
| Initial physical VFS import reads only the main database and does not reconcile an outstanding WAL | [SQLite Integration](./graft-sqlite-integration-1.0.md#13-known-limits) |
| Browser/WASM does not currently implement remote synchronization or load the native Node addon | [Runtime and Adapters](./graft-runtime-adapters-1.0.md#12-browserwasm-profile) |

Changing any row from a disclosed limit into a supported guarantee requires
implementation evidence and a conformance update; deleting the warning alone
does not change behavior.

## Change policy

Compatible clarifications MAY add examples, test vectors, limits, or links.
Changes to persisted formats, object identity, merge outcomes, wire protocol,
operation postconditions, failure atomicity, or whether an operation can
replace a worktree file require a new version or an explicitly documented
compatible extension. Private refactoring does not require a specification
change when observable behavior remains unchanged.
