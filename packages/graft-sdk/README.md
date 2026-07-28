# Graft SDK for Node.js and Electron

`@eidos.space/graft` embeds Graft as a long-lived, in-process repository SDK for Node.js and
Electron. It is the native application integration surface used by Eidos Lite Desktop. The Rust
session and Node-API crates remain repository-internal implementation crates and are not published
to crates.io.

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
Electron utility process
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

The root package selects one optional native package for the current host. Release `0.1.x`
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
platform/architecture-specific. An Electron package must copy it outside ASAR, or list `*.node`
under `asarUnpack`, and load it in the utility process. `GRAFT_SDK_NATIVE_PATH` may point tests or
packaging harnesses at an explicit addon path.

Published packages use the standard native optional-dependency model: the root package contains
the JavaScript wrapper, declarations, and README; each platform package contains exactly one
`.node` binary plus `os`, `cpu`, and (on Linux) `libc` constraints. Consumers do not need a Rust
toolchain or system SQLite library, and the package has no install script.

## Use

```js
const { RepositorySession } = require("@eidos.space/graft")

const session = await RepositorySession.open(spaceRoot)
try {
  const status = await session.status()
  const diff = await session.diff({ rows: true })
} finally {
  await session.close()
}
```

Eidos Lite should keep one session per open Space in its utility process rather than constructing a
session per command:

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

The Electron main/renderer boundary can continue to use typed IPC, but the utility process must
load this package directly. It must not launch `graft`, host a long-running CLI daemon, or proxy
commands over JSON stdin.

## API and worktree materialization

“Materializes worktree” means that an operation can replace, create, or remove physical tracked
files in the Space. Changes confined to `.graft` are not counted.

| Method | Purpose | Materializes tracked worktree files |
| --- | --- | --- |
| `open`, `close`, `reopen` | Manage the retained repository runtime | No |
| `init` | Initialize `.graft` metadata | No |
| `status` | Inspect worktree and index state | No |
| `addAll` | Read/import the current worktree into the index | No |
| `commit` | Advance repository history | No |
| `diff` | Compare worktree, index, or revisions | No |
| `history` | Read commit history and status | No |
| `restore` | Replace selected paths from a revision | **Yes** |
| `configureRemote` | Persist remote URL/upstream metadata | No |
| `push` | Send objects/refs to a remote | No |
| `fetch` | Receive objects/refs into `.graft` | No |
| `pull` | Fetch, integrate, and check out the result | **Yes** |
| `cloneRepository` | Populate a new worktree from a remote | **Yes** |

`operationMaterializesWorktree(name)` exposes this contract to the Eidos gate. Before `restore`,
`pull`, or `cloneRepository`, Eidos must checkpoint and close application SQLite handles for paths
that can be replaced. Reopen those application handles after the SDK promise settles. The Graft
repository session itself stays open during the operation.

`addAll` reads SQLite files and their committed/WAL state but does not replace them. Eidos should
still checkpoint its application databases before snapshotting when it needs a deterministic
commit boundary.

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

If an Electron utility process crashes, the OS releases its storage lock. A replacement utility
process creates a new session and calls `open()`; Graft reconstructs the runtime from durable
repository state. No stale daemon registration or PID file is involved.

Dropping the JavaScript native object releases its Rust `Arc`, but Eidos should always await
`close()` during orderly Space shutdown so lifecycle errors are observable.

## Cancellation, conflicts, and errors

Every asynchronous method accepts `{ signal }`. Aborting cancels work that is still queued in the
Node/libuv async-work queue. Once a Graft operation has started, it is not preempted: its result or
error wins, avoiding partially interrupted repository mutations. Closing likewise waits for the
in-flight operation instead of canceling it.

Repository conflicts and command results retain Graft's existing JSON schema. The binding
stabilizes transport/lifecycle failures as `GraftSdkError` with codes such as:

- `GRAFT_SDK_SESSION_CLOSED`
- `GRAFT_SDK_SESSION_CLOSING`
- `GRAFT_SDK_REPOSITORY_BUSY`
- `GRAFT_SDK_INVALID_ARGUMENT`
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
cargo build --release -p graft-tool
pnpm --dir packages/graft-sdk bench
```

`GRAFT_SDK_BENCH_ITERATIONS` controls repetition (default 30), and `GRAFT_CLI_PATH` may select a
specific CLI binary. The benchmark reports first/min/p50/p95/max/mean for:

- SDK `open + status + close` cold calls
- repeated hot-session SDK `status` and `diff`
- CLI-process `status` and `diff`

The benchmark deliberately closes the retained SDK session before each CLI sample because the
repository lock correctly excludes a second writer/runtime.

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
packages before the root package, creates a GitHub SDK release with checksums, then installs the
public root package on every supported platform and opens a repository session. After each npm
publish, the job allows up to ten minutes for the immutable version to become visible through the
registry read path before it advances to the next package.

See [`RELEASE.md`](https://github.com/eidos-space/graft/blob/main/RELEASE.md) for the first-publish
credential bootstrap, npm trusted publisher configuration, partial-release recovery, and
post-publish checks.
