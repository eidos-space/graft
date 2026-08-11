# Graft Runtime and Adapters 1.0

Status: Draft implementation-aligned normative baseline
Version: 1.0
Published: 2026-08-11
Canonical language: English

## Abstract

This specification defines the shared repository command service, one-shot CLI
adapter, retained Rust SDK session, Node-API/JavaScript binding, browser/WASM
host, operation/result mapping, lifecycle, concurrency, cancellation, caching,
limits, and stable SDK errors.

The CLI and SDK are **not two repository implementations**. They are adapters
over the same `RepositoryCommandService`, repository core, SQLite merge/diff
code, and official remote implementations. Their supported command surfaces
and process lifetimes differ, but canonical repository behavior does not.

## 1. Architecture and ownership

```text
CLI parser (one command) -----------+
                                    |
Rust RepositorySession (retained) --+--> RepositoryCommandService
          ^                         |           |
          |                         |           +--> Repository core/runtime
Node-API 8 async class --> JS API --+           +--> SQLite diff/merge/worktree
                                                +--> official remotes

Browser UI --> Worker --> real graft-cli WASM --> same Rust command paths
```

`RepositoryCommandService` is the control-plane owner for command execution.
It opens/retains repository runtime and storage coordination, parses no UI
state, and executes typed `RepositoryCommand` values directly. It does not open
a SQLite connection or route production repository operations through PRAGMA.

Repository semantics belong to their domain specifications; this document owns
adapter mapping and host lifecycle.

## 2. Command service

### 2.1 Opening and target resolution

The service target may be a repository `.graft` directory, a path inside a
worktree, or a database path whose containing repository can be discovered.
Opening constructs the repository-scoped runtime registry once, acquires the
underlying local storage coordination, and associates a credential policy.

The one-shot helper opens a service, executes one parsed command, and drops it.
A retained SDK session keeps the same service/runtime for subsequent commands.
Both routes MUST call the same command behavior and produce equivalent
repository state for equivalent typed inputs.

### 2.2 Typed boundary

String/CLI parsing occurs at the adapter boundary. Once constructed, a command
is typed; the service MUST NOT reinterpret arbitrary SQL or PRAGMA text. Typed
merge helpers likewise map revision, side, path, table, and row identity to the
same internal merge operations used by the CLI.

JSON-producing commands return UTF-8 JSON. The service and bindings MUST treat
invalid internal JSON as an adapter error, not return it as successful output.

## 3. CLI profile

The CLI is a one-shot adapter with environment credential policy. Its public
surface currently includes:

```text
id, log, init, sql, clone, status, audit, gc, ls-files, payload, config,
add, rm, commit, diff, show, checkout, restore, export, reset,
branch, tag, switch, merge, conflicts, resolve,
remote, ls-remote, fetch, pull, push
```

Nested commands provide branch/tag/remote/config/payload management and merge
continue/abort/resolution. `--json` output is the automation contract when
offered; human text is presentation and MAY evolve compatibly.

The CLI also contains hidden host commands such as `browser-move` and
`merge-api`. Hidden commands are adapter plumbing, not a separate repository
protocol. `merge-api` exposes SDK-shaped merge calls to the browser build using
the real Rust session/core.

CLI-only capabilities need not be present in the SDK. In particular, the SDK
1.0 surface does not expose every branch, tag, switch, reset, audit, payload,
export, GC, SQL shell, or low-level remote-list command.

## 4. Retained Rust session

### 4.1 Lifecycle

The session lifecycle is:

```text
closed -> opening -> open -> closing -> closed
                    ^          |
                    +-- reopen-+
```

`open` from `closed` constructs the retained service. Opening an already-open
session fails with `SESSION_ALREADY_OPEN`. Calls during `opening` or `closing`
fail with the corresponding lifecycle code. `close` publishes `closing`, waits
for the currently executing operation, rejects queued operations after they
acquire the session gate, releases the service/cache, and becomes `closed`.
Closing an already-closed session is idempotent.

`reopen` drops and reconstructs the runtime from durable repository state. It
is the supported way to prove close/reopen recovery, including active merge
state. Failure to reopen leaves the session closed.

### 4.2 Concurrency

One retained session serializes complete operations with a mutex. A running
operation may finish while close waits; queued work does not sneak through
after closing begins. Separate sessions for different repositories may run in
parallel. Separate live sessions/processes targeting the same storage are
subject to repository/storage locks and normally return repository-busy.

There is no global SDK mutex. Hosts SHOULD maintain at most one live session
per canonical repository/Space identity while allowing unrelated repositories
to proceed independently.

### 4.3 Durable state

The session itself is not canonical state. Process crash or finalizer drop
releases OS-backed resources; a new session reconstructs from `.graft`, object
storage, index, refs, and durable merge records. No daemon lease, stdin stream,
socket, or PID registry is required for recovery.

## 5. Rust/Node SDK operation surface

Shared CLI/SDK domains map as follows. A blank SDK cell means CLI-only in the
current contract, not a second implementation.

| CLI/domain | Rust/JavaScript session | Result emphasis |
| --- | --- | --- |
| `init` | `init` | repository format/layout outcome |
| `status --json` | `status`, `statusIncremental` | full status; incremental generation/token/telemetry |
| `add --all`, `add <paths>` | `addAll`, `stagePaths` | staged/affected paths; expected-head guard for explicit batch |
| completed host rename, `rm`/untrack | `recordPathMove`, `untrackPaths` | exact previous/current or affected paths |
| `commit` | `commit` | commit/ref outcome; non-materializing |
| `diff` | `diff`, `diffPaths`, `diffSqlitePaths`, `readPathContent` | general JSON or typed bounded paths/rows/content |
| `log`, `show` | `history`, `historySummaries`, `commitDetails`, `commitChangedPaths` | lazy metadata/details/path pages |
| `ls-files`, ignore checks | `inventory`, `isIgnoredPath(s)` | bounded path classification |
| repository/ref metadata, `remote list` | `repositoryMetadata`, `listRemotes` | credential-redacted metadata |
| `restore` | `restore`, `restorePaths` | affected paths and checkout outcome |
| `remote add/set-url` subset | `configureRemote` | local config/upstream outcome |
| `fetch`, `push`, `pull` | same method names | remote/ref plus merge outcome where applicable |
| `clone` | `cloneRepository` | new repository/checkout outcome |
| `merge` + `conflicts` + `resolve` | `planMerge`, `applyMerge`, merge status/list/version, path/row/text resolution, `continueMerge`, `abortMerge` | typed plan/state tokens plus command outcome |
| branch/tag/switch/reset/export/audit/payload/GC/SQL/low-level remote commands | — | CLI-only 1.0 surface |

The stable operation classification is:

```text
Init, Status, StatusIncremental, AddAll, StagePaths, RecordPathMove,
UntrackPaths, Commit, Diff, DiffPaths, ReadPathContent, History,
HistorySummaries, CommitDetails, CommitChangedPaths, IsIgnoredPath,
IsIgnoredPaths, Inventory, RepositoryMetadata, ListRemotes, Restore,
RestorePaths, RemoteConfigure, Push, Fetch, Pull, Clone, PlanMerge,
ApplyMerge, GetMergeStatus, ListMergePaths, ListMergeConflicts,
ReadMergeVersion, SetMergePathResult, ResolveMergeRow,
WriteAndStageTextResult, ContinueMerge, AbortMerge
```

The JavaScript class maps these to camelCase methods, with `diffSqlitePaths`
as the typed bounded SQLite-diff specialization and `cloneRepository` avoiding
the reserved/general meaning of `clone`. `packages/graft-sdk/index.d.ts` is the
canonical JavaScript type contract for exact option and result fields.

Inputs use JavaScript camelCase. Parsed JSON results preserve repository
snake_case field names, including `expected_head`, `plan_token`, `state_token`,
`materializes_worktree`, and telemetry fields. Bindings MUST NOT silently rename
individual result fields in a way that diverges from `index.d.ts`.

### 5.1 Compare-and-swap inputs

Mutations that can race accept `expectedHead`, plan tokens, or merge state
tokens as applicable. Omission is allowed only where the typed contract says
so, such as an unborn head. A mismatch maps to repository stale/command error
and produces no hidden overwrite.

### 5.2 Result categories

Adapters return either:

- typed status/history/diff/inventory/metadata/merge pages;
- a parsed `GraftJson` command outcome for legacy/general operations; or
- a stable lifecycle/string primitive.

Batch path results identify affected paths and per-path outcomes. Paged results
return `has_more`/`next_cursor` or an equivalent documented shape. Content
states distinguish UTF-8, too-large, missing-payload, invalid-UTF8, and absent.

## 6. Public limits and validation

Current SDK limits are:

| Input/result | Limit |
| --- | --- |
| history summary page | 500 |
| changed-path page | 100 |
| diff path page | 100 |
| explicit diff path request | 10,000 |
| path/merge UTF-8 content | 8 MiB |
| batch path mutation | 1,000 |
| inventory page | 1,000 |
| ignore query paths | 1,000 |
| merge paths page | 500 |
| merge conflicts page | 1,000 |
| SQLite row page | 1,000 |

JavaScript defaults are 50 commits for `history`/`historySummaries`, 100 items
for changed-path, diff, inventory, and merge listing where a limit is omitted,
100 rows for a typed SQLite row page, and inventory kind `tracked_ignored`.
`readPathContent` and `readMergeVersion` require an explicit `maxBytes` rather
than silently selecting an unbounded read.

Bindings MUST reject missing required options, invalid unions, out-of-range
limits, invalid paths/revisions/row identities, and mutually exclusive diff
modes before ambiguous execution. An empty explicit mutation batch is rejected
unless its operation explicitly defines a no-op.

## 7. Incremental status cache

The retained SDK keeps an in-memory classification cache and MAY persist
content-addressed status snapshots under `.graft/cache/sdk-status`. Current
persistent schema version is 3, with at most four snapshots; each accepted or
newly persisted snapshot is limited to 256 MiB.

A cache identity covers SDK schema, repository/object format, `HEAD`, index,
refs, config, ignore sources, tracked paths, and visible-untracked metadata/
content fingerprints needed for classification. Before returning a persistent
hit, the SDK rechecks relevant fingerprints. Any mismatch, missing input,
schema difference, or corrupt snapshot invalidates the whole candidate.

Writes use a same-directory temporary file, file sync, atomic rename, and
directory sync. Absolute worktree paths and credentials MUST NOT be serialized.
Cache deletion/corruption can only reduce performance; full status remains the
source of truth.

Incremental status returns a generation, change token, full status, and
telemetry including duration, examined paths, metadata/tree/status cache hits,
persistent load/save, and stability retries.

## 8. Worktree stability and materialization gate

During path traversal, the SDK samples relevant file/directory/rename/unlink/
symlink state and retries a racing observation up to three times. Failure to
obtain one coherent view returns `GRAFT_SDK_REPOSITORY_STALE`; raw platform
`ENOENT`, `ENOTDIR`, or `EISDIR` races SHOULD NOT leak as the public diagnosis.

`operationMaterializesWorktree(name)` is a conservative host gate. It returns
true for:

```text
Restore, RestorePaths, Pull, Clone, ApplyMerge,
SetMergePathResult, ResolveMergeRow, WriteAndStageTextResult,
ContinueMerge, AbortMerge
```

It means the operation **may** create, replace, or remove tracked physical
paths. It does not prove that a particular invocation did so. Exact handle,
WAL, and replacement behavior is owned by [Graft Worktree Materialization
1.0](./graft-worktree-materialization-1.0.md).

Notably, staging and `commit` are non-materializing. Older configuration,
snapshot, Playground, or SDK architecture prose that says commit rematerializes
SQLite is stale and MUST NOT override the code/test contract.

## 9. Cancellation

Rust operations accept a cooperative cancellation token. Node methods accept
an optional `AbortSignal`; Node can cancel queued async work, while started work
also observes the shared token at repository traversal, diff/history, staging,
restore, hydration, and SQLite page/row checkpoints.

Cancellation before a mutation's durable boundary leaves no effect. Some
multi-path operations may complete a valid prefix before observing
cancellation; each completed index write or individual worktree replacement
remains visible in subsequent status. Remote publication after request send may
have an uncertain outcome and follows Remote Sync recovery semantics.

Cancellation does not poison the retained session. JavaScript maps the stable
SDK cancellation code to an `AbortError`.

## 10. Stable errors

Rust and Node expose these codes:

```text
GRAFT_SDK_SESSION_CLOSED
GRAFT_SDK_SESSION_OPENING
GRAFT_SDK_SESSION_CLOSING
GRAFT_SDK_SESSION_ALREADY_OPEN
GRAFT_SDK_REPOSITORY_BUSY
GRAFT_SDK_CANCELLED
GRAFT_SDK_INVALID_ARGUMENT
GRAFT_SDK_INVALID_RESPONSE
GRAFT_SDK_REPOSITORY_STALE
GRAFT_SDK_REMOTE_TRANSPORT_TIMEOUT
GRAFT_SDK_REMOTE_PUBLICATION_UNCONFIRMED
GRAFT_SDK_REMOTE_PUBLICATION_OUTCOME_UNKNOWN
GRAFT_SDK_REPOSITORY_COMMAND
```

The JavaScript wrapper throws `GraftSdkError` with `code` and optional cause,
except cancellation, which becomes `AbortError`. Unknown native-loader failures
may use a binding-level code and MUST clearly identify missing/unsupported
native packages.

Error messages MUST redact configured in-memory credentials. Adapters MUST
preserve meaningful repository/remote conflict detail instead of collapsing
every failure into one generic exception.

## 11. Node-API and package boundary

`graft-sdk-node` uses Node-API 8. Each asynchronous call runs native work off
the JavaScript main thread and owns the Rust session through a shared lifetime.
Node-API provides ABI stability, while binaries remain platform/architecture/
libc specific.

The CommonJS root npm package requires Node.js 20 or newer and loads, in order,
the path named by `GRAFT_SDK_NATIVE_PATH`, a colocated native binary, or an
exact platform optional package. Supported package families
currently cover macOS arm64/x64, Linux glibc arm64/x64, and Windows x64. There
is no browser fallback, install-time compilation, or remote binary download.
Electron hosts keep `.node` outside ASAR or unpack it and SHOULD use a utility
process rather than the renderer.

## 12. Browser/WASM profile

The Playground compiles the real `graft-cli` Rust binary for Emscripten and
runs it off the UI thread:

```text
React UI -> message RPC -> Web Worker -> callMain(graft.wasm)
                                      -> WasmFS / OPFS worktree
                                      -> memory-backed /tmp
```

OPFS backs the persistent browser repository. `/tmp` is memory-backed so
temporary SQLite diff/merge databases do not leak into the worktree. The host
MUST provide COOP/COEP isolation headers required by the runtime and MUST keep
the bundled WASM version aligned with the source/tool manifest.

The currently verified Emscripten toolchain is 6.0.3. A different compiler MAY
be used only when the generated Wasm features, WasmFS OPFS backend, worker
startup, and conformance tests remain compatible.

The Playground merge UI calls hidden `merge-api` commands that construct/use
the real Rust repository session contract. Fixtures and browser tests are
therefore core integration evidence, not pure mocks. UI state is a projection
and MUST reload durable merge status after worker/session restart.

Current browser limitations are explicit:

- the native Node SDK package and `.node` addon cannot load in a browser;
- remote synchronization is intentionally unavailable in the WASM build;
- browser filesystem/handle behavior is composed through OPFS/WasmFS; and
- only operations compiled and bridged by `graft-cli` are available.

A browser profile MUST not claim native SDK or remote conformance for an
unavailable operation. Test doubles may test UI states but are not evidence of
repository behavior.

## 13. Conformance requirements

`GRAFT-CLI-1.0` and `GRAFT-SDK-1.0` implementations MUST test:

1. CLI and SDK equivalence for shared typed operations;
2. absence of SQL/PRAGMA control-plane dependence;
3. lifecycle transitions, queued-work rejection, and reopen recovery;
4. same-session serialization and same-repository busy behavior;
5. option/limit/type validation against the TypeScript contract;
6. result JSON parsing and stable field names;
7. cache hit equivalence, invalidation, atomic persistence, and redaction;
8. path-race stale errors and retry bounds;
9. cancellation before/after safe mutation boundaries;
10. stable error mapping and credential redaction;
11. conservative materialization classification; and
12. Node async execution and native-package selection.

`GRAFT-Browser-1.0` additionally tests the real WASM command, OPFS persistence,
memory-backed temporary storage, worker restart/reopen, merge recovery, version
manifest, and unavailable-capability disclosure.

Current evidence lives in `crates/graft-sqlite/src/repo_service.rs`,
`crates/graft-sdk`, `crates/graft-sdk-node`, `packages/graft-sdk`, CLI tests, and
`web-demo` worker/unit/Playwright tests.

## 14. Compatibility notes and current drift

- `docs/sdk-architecture.md` now records the current direct command-service
  boundary. Copies from before this baseline that describe repository PRAGMAs
  are stale.
- CLI and SDK share implementation, but their public operation sets are not
  identical. Absence from the SDK is a capability boundary, not alternate
  semantics.
- `index.d.ts` is the Node/JavaScript contract. Rust-private types and CLI human
  output are not substitutes for it.
- Browser `merge-api` is a host adapter to real Rust code; remote sync remains
  unavailable and must not be mocked into a conformance claim.
