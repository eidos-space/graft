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

CLI commands whose complete read set is limited to atomically replaced refs
and immutable repository objects MAY bypass the retained runtime and storage
lock. `graft log` MUST use this metadata-only path and remain readable while a
live session owns the same repository's storage lock. This exception MUST NOT
be extended to commands that inspect or mutate the index, worktree, snapshots,
remotes, or runtime storage without equivalent concurrency evidence.

`graft status` MAY bypass runtime storage only by loading a complete persisted
status classification and revalidating the cache identity and all relevant
worktree fingerprints described in section 7. A missing, stale, racing, or
corrupt proof MUST fall back to authoritative runtime-backed status and MUST
NOT be presented as current. Index writers MUST use same-directory temporary
files and atomic replacement so concurrent readers cannot observe partial
serialization.

### 4.3 Durable state

The session itself is not canonical state. Process crash or finalizer drop
releases OS-backed resources; a new session reconstructs from `.graft`, object
storage, index, refs, and durable merge records. No daemon lease, stdin stream,
socket, or PID registry is required for recovery.

An SDK session MAY carry an in-memory `identity` override with `name` and
`email`. The override applies to commits and ref updates executed by that
session, survives `close`/`reopen` on the same session object, and MUST NOT be
written to repository configuration. When absent, the session uses the
repository's configured `user.name` and `user.email`, then the repository
defaults. Persistent identity changes use the repository configuration API.

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
| `config get/list/set/unset` | `configGet`, `configList`, `configSet`, `configUnset` | effective repository configuration entries |
| `restore` | `restore`, `restorePaths` | affected paths and checkout outcome |
| `remote add/set-url` subset | `configureRemote` | local config/upstream outcome |
| `remote remove` | `removeRemote` | idempotent local disconnect; preserves worktree and local history |
| `fetch`, `push`, `pull` | same method names | remote/ref plus merge outcome where applicable |
| `clone` | `cloneRepository` | new repository/checkout outcome |
| `merge` + `conflicts` + `resolve` | `planMerge`, `applyMerge`, merge status/list/version, path/row/text/provider resolution, `continueMerge`, `abortMerge` | typed plan/state/provider tokens plus command outcome |
| branch/tag/switch/reset/export/audit/payload/GC/SQL/low-level remote commands | — | CLI-only 1.0 surface |

`listRemotes` distinguishes encrypted HTTP configuration with kind
`encrypted_http`. This is public profile metadata; it does not indicate that a
session is unlocked or that an adapter implements encrypted enrollment. Its URL
projection excludes credentials, passwords, keys and host staging state.

`removeRemote(name)` MUST remove only that named remote, its upstream configuration
and remote-tracking refs, and clear its in-memory HTTP credential. A missing remote
MUST succeed with `removed: false`. Local files (including unsaved edits), commits,
local branches, tags and other remotes MUST be preserved. The host owns encrypted
authority revocation and draining before disconnecting a connection.
An attached encrypted authority that has not disconnected MUST cause rejection
before durable remote configuration is removed; locking alone is insufficient.

The native Rust SDK's `RepositorySession::new_with_remote_credentials` accepts
explicit shared `RemoteCredentials` and an optional identity override. The native
`e2ee` feature permits host-registered encrypted sessions through that provider.
Repository runtime close/reopen does not disconnect a secret session shared with
other repositories. The host owns secret-session locking, draining and credential
disconnect; this constructor does not enroll a remote or remember its key.

With the native `e2ee` feature, `graft_sdk::e2ee` exposes the core's opaque secret
session, checkpoint and trusted-provider interfaces. Hosts may explicitly use
`EncryptedHttpClient::unlock_remembered` to retrieve the pinned remote's public
descriptor/config, load its bound remembered root, authenticate config and persist
the checkpoint before admitting data operations. Missing/unavailable storage and
identity/authentication/checkpoint failures preserve the locked state. Online
unlock never saves or deletes credentials. The caller's cancellation and session
lock retain the core task-draining contract. This Rust surface does not expose a
Node secret API by itself or approve a host/platform remembering claim.

The Rust encrypted HTTP client also exposes `publish_initial_config` and the
non-mutating `reconcile_initial_config`. These take a trusted
`InitializationJournal`; the host first unlocks from its exact local candidate.
`InitialConfigPublicationOutcome::Confirmed` means config-published after exact
authenticated readback and durable journal advancement. It does not mean
connected or publish initial objects/refs. Known published config disappearance
never triggers another create. The native `FileInitializationJournal` requires a
private directory, explicit create/open and owner-only records. Its blocking
persistence stays in the key-owning operation's draining boundary. Complete
enrollment, platform storage, host authentication and CLI/Node orchestration
remain separate requirements of E2EE Remote section 17.

The Node binding's opt-in `e2ee` feature adds a trusted-host
`EncryptedRemoteSession`, with an immutable remote name/URL/UUID and host-verified
authentication domain/account subject. Constructor options select explicit
checkpoint creation or existing opening; missing existing state is not recreated.
Native checkpoints support Unix owner-only modes and Windows protected
owner/SYSTEM-only DACLs, native identity checks and file locks. Hosts can allocate
new private directories with `EncryptedRemoteSession.createPrivateDirectory(path)`.
Windows permits session-only password unlock without a remembered-key provider.
`macos-keychain` additionally permits the approved native provider; unavailable
storage never falls back. Native builds without `e2ee` do not enable this API.

`unlock(Buffer)` consumes and clears its caller-owned, exclusive password Buffer
synchronously, registers the unlock before async scheduling and retains only a
controlled native copy. It rejects strings, invalid UTF-8 and out-of-bound input;
the API cannot promise to erase prior JS copies. No key/password JSON is returned.
`unlockRemembered`, `remember`, `lock`, `pause`, `disconnect`, `reconnect`,
`refreshBearerToken` and `forget` preserve core lifecycle/storage contracts.
Dropping a JS Promise is not cancellation; explicit lock/disconnect cancels and
drains admitted work. Account switching requires a new identity-bound handle.
Hosts must verify refreshed token identity and coordinate every session sharing
an OS entry. Ordinary plugin RPC must not expose this authority.

`RepositorySessionOptions.encryptedRemote` shares that handle's explicit
credentials with its retained runtime. `addEncryptedRemote` adds its typed config
without overwrite or profile inference; `cloneEncryptedRemote` uses the same
typed clone/destination contract. Runtime close/reopen does not lock the shared
authority. Repository methods retain their existing AbortSignal contract. Remote
enrollment, password update/recovery authorization and signed OS-host remembering
evidence remain separate delivery requirements.

`clone_repository_with_config` accepts a typed `RemoteConfig` and optional branch,
using the same command-service clone and destination safety contract as the URI
API. An encrypted config preserves its profile/UUID and uses the host's registered
`origin` session before network data access. It does not infer encryption from a
plain URL or accept keys in the config. HTTP credentials, query parameters and
fragments in URLs are rejected before destination initialization. This is a Rust
SDK entry point; Node/CLI enrollment remains separately required.

The stable operation classification is:

```text
Init, Status, StatusIncremental, AddAll, StagePaths, RecordPathMove,
UntrackPaths, Commit, Diff, DiffPaths, ReadPathContent, History,
HistorySummaries, CommitDetails, CommitChangedPaths, IsIgnoredPath,
IsIgnoredPaths, Inventory, RepositoryMetadata, ListRemotes, ConfigGet,
ConfigList, ConfigSet, ConfigUnset, Restore,
RestorePaths, RemoteConfigure, RemoteRemove, Push, Fetch, Pull, Clone, PlanMerge,
ApplyMerge, GetMergeStatus, ListMergePaths, ListMergeConflicts,
ReadMergeVersion, DiffMergeSqlite, SetMergePathResult, ResolveMergeRow,
ResolveMergeCell, ResolveMergeTable, StageMergeSqliteResult,
PrepareSemanticMerge, RecordSemanticMergeConflicts,
AcceptSemanticMergeResult, UnresolveMergePath, WriteAndStageTextResult,
ContinueMerge, AbortMerge
```

The JavaScript class maps these to camelCase methods, with `diffSqlitePaths`
as the typed bounded SQLite-diff specialization and `cloneRepository` avoiding
the reserved/general meaning of `clone`. `packages/graft-sdk/index.d.ts` is the
canonical JavaScript type contract for exact option and result fields.

`push`, `fetch`, `pull`, `cloneRepository`, and `applyMerge` accept an optional
`onProgress` callback. Its events report cumulative HTTP body bytes for the
current operation, a direction of `upload` or `download`, and `totalBytes` when
the transport provides a trustworthy length. A host MUST treat an absent total
as indeterminate and MUST NOT derive a percentage from command phases.
For `applyMerge`, events cover only snapshot hydration required by the guarded
plan; an already hydrated plan may emit no transfer event.

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

The CLI MAY consume a validated snapshot without opening a second runtime. It
MUST apply the same validation as the SDK, refresh repository/ref projection,
and preserve the canonical human and JSON status shapes. Cache miss or
invalidation continues through full status rather than returning a partial or
optimistic clean result.

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

### 9.1 Transfer progress

Progress counts bytes consumed by an HTTP upload body or received from an HTTP
download body. Multiple requests and retries in one operation are cumulative;
the total grows as their lengths become known. Events are rate-limited during
transfer and include a final event for a successfully consumed body. Progress
is observational: callbacks do not change publication, recovery, cancellation,
or error semantics, and callback delivery does not make an uncertain remote
publication certain.

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

Encrypted operations MUST classify typed causes before message-based fallback.
Rust exposes `SdkErrorCode::Encrypted(EncryptedErrorCode)`; its `as_str()` and
Node's `GraftSdkError.code` use the `GRAFT_SDK_E2EE_` family. This classification
also applies to repository push/fetch/clone, not only secret-session methods.
An unavailable encrypted feature returns
`GRAFT_SDK_E2EE_UNSUPPORTED_ENCRYPTED_PROFILE`, including in default builds.

| Code | Meaning and host handling |
| --- | --- |
| `GRAFT_SDK_E2EE_LOCKED` | Explicit unlock is required; this is not a repository writer lock. |
| `GRAFT_SDK_E2EE_DISCONNECTED` | Explicit reconnection is required before unlock. |
| `GRAFT_SDK_E2EE_PAUSED` | Sync admission is paused; the host retains the user's pause intent. |
| `GRAFT_SDK_E2EE_AUTHENTICATION_FAILED` | HTTP 401/403 account authentication or authorization failure. |
| `GRAFT_SDK_E2EE_INTEGRITY_FAILED` | Cryptographic authentication of encrypted data failed; account token refresh cannot repair it. |
| `GRAFT_SDK_E2EE_UNLOCK_FAILED` | Password unwrap failed; does not distinguish an incorrect password from damaged wrapping data. |
| `GRAFT_SDK_E2EE_CONFIG_ROLLBACK_DETECTED` | Observed config violates the retained checkpoint. |
| `GRAFT_SDK_E2EE_CHECKPOINT_UNAVAILABLE` | Required trust state cannot be read or durably advanced. |
| `GRAFT_SDK_E2EE_REMEMBERED_KEY_MISSING` | No remembered entry exists for the bound identity. |
| `GRAFT_SDK_E2EE_SECURE_STORAGE_UNAVAILABLE` | Approved secure storage is unavailable or unauthorized. |

The core's cryptographic `AuthenticationFailed` maps to SDK `INTEGRITY_FAILED`;
it MUST NOT be confused with HTTP account authentication. Hosts MUST NOT silently
reset trust state, overwrite remote data or switch to plaintext after integrity,
identity or rollback failures. Remaining typed encrypted causes retain distinct
codes from `EncryptedErrorCode`; messages are diagnostic, not a dispatch API.
Publication outcome unknown/unconfirmed takes precedence over nested cancellation
or transport failures and retains the existing `GRAFT_SDK_REMOTE_PUBLICATION_*`
codes. Only an ordinary cancellation becomes `GRAFT_SDK_CANCELLED`/`AbortError`.

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
Packaged Node.js applications MUST keep the selected `.node` file accessible
to Node's module loader.

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
10. cumulative known/unknown-length HTTP transfer progress, upload-bundle
    declared totals, and final events;
11. stable error mapping and credential redaction;
12. conservative materialization classification; and
13. Node async execution and native-package selection.

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
