# Graft Runtime and Adapters 1.0（中文参考）

状态：与当前实现对齐的规范草案
版本：1.0
发布日期：2026-08-11
规范语言：英文

## 摘要

本规格定义 shared repository command service、one-shot CLI、retained Rust SDK、
Node-API/JavaScript binding、browser/WASM host、operation/result mapping、lifecycle、
concurrency、cancellation、cache、limit 与稳定错误。

CLI 和 SDK **不是两套 repository 实现**：它们都调用同一
`RepositoryCommandService`、core、SQLite diff/merge 与 official remote。差异是
公开 surface 与 process lifetime，不是 canonical behavior。

## 1. 架构

```text
CLI parser ------------------------+
Rust RepositorySession ------------+--> RepositoryCommandService
Node-API async -> JavaScript -------+          +--> core/runtime
                                               +--> SQLite diff/merge/worktree
                                               +--> official remotes

Browser UI -> Worker -> real graft-cli WASM -> 同一 Rust command path
```

Command service 直接执行 typed `RepositoryCommand`，retain runtime/storage lock；
它不打开 SQLite connection，也不通过 SQL 路由 repository command。

## 2. Command service

Target 可以是 `.graft`、worktree 内路径或可 discover repository 的 database path。
Open 一次构造 repository-scoped runtime registry、获取 coordination、绑定 credential
policy。One-shot helper 每次 open/execute/drop；retained session 复用 service/runtime。
相同 typed input 必须产生等价 repository state。

String/CLI parse 只发生在 adapter boundary；进入 service 后是 typed command，不能
重新解释任意 SQL/PRAGMA。JSON command 必须产生合法 UTF-8 JSON，否则是 adapter
error。

## 3. CLI profile

CLI 当前 public surface：

```text
id, log, init, sql, clone, status, audit, gc, ls-files, payload, config,
add, rm, commit, diff, show, checkout, restore, export, reset,
branch, tag, switch, merge, conflicts, resolve,
remote, ls-remote, fetch, pull, push
```

Nested command 管理 branch/tag/remote/config/payload 与 merge continue/abort。存在
`--json` 时它是 automation contract；human text 可以兼容演进。

Hidden `browser-move`、`merge-api` 是 host plumbing，不是另一协议。SDK 1.0 不暴露
CLI 的全部 branch/tag/switch/reset/audit/payload/export/GC/SQL 等能力。

## 4. Retained Rust session

```text
closed -> opening -> open -> closing -> closed
                    ^          |
                    +-- reopen-+
```

`open` 构造 retained service；already-open、opening、closing 分别返回稳定 lifecycle
error。`close` 先发布 closing，等待当前操作，拒绝 queued work，释放 service/cache；
重复 close 幂等。`reopen` 丢弃并从 durable repository state 重建，失败后 closed。

同一 session 用 mutex 串行完整操作；close 等 running call，但 queued call 不能穿过。
不同 repository 可并行；同 repository 的不同 live session/process 由 storage lock
返回 busy。没有 global SDK mutex，host 应按 canonical Space/repository identity 只
保留一个 live session。

Session 不是 canonical state。Crash/finalizer 后新 session 从 `.graft`、objects、
index、refs 与 merge records 恢复，不需要 daemon/socket/PID registry。

## 5. SDK operation surface

CLI 与 SDK 主要映射如下；SDK 空白表示当前 CLI-only，不是另一套实现：

| CLI/domain | Rust/JavaScript session | 结果重点 |
| --- | --- | --- |
| `init` | `init` | format/layout |
| `status --json` | `status`, `statusIncremental` | full status、generation/token/telemetry |
| `add --all`/paths | `addAll`, `stagePaths` | staged/affected paths、expected head |
| host rename/untrack | `recordPathMove`, `untrackPaths` | exact previous/current/affected paths |
| `commit` | `commit` | commit/ref；non-materializing |
| `diff` | `diff`, `diffPaths`, `diffSqlitePaths`, `readPathContent` | bounded path/row/content |
| `log`, `show` | `history`, `historySummaries`, `commitDetails`, `commitChangedPaths` | lazy metadata/detail/path page |
| `ls-files`/ignore | `inventory`, `isIgnoredPath(s)` | bounded classification |
| metadata/`remote list` | `repositoryMetadata`, `listRemotes` | credential-redacted metadata |
| `restore` | `restore`, `restorePaths` | affected paths/checkout outcome |
| remote config subset | `configureRemote` | local config/upstream |
| `fetch/push/pull` | 同名 method | remote/ref/merge outcome |
| `clone` | `cloneRepository` | new repository/checkout |
| merge/conflicts/resolve | plan/apply/status/list/version/path/row/text/provider/continue/abort | typed plan/state/provider token 与 outcome |
| branch/tag/switch/reset/export/audit/payload/GC/SQL/low-level remote | — | CLI-only 1.0 |

稳定分类：

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

JavaScript 使用 camelCase method；`diffSqlitePaths` 是 typed bounded SQLite diff，
`cloneRepository` 避免歧义。`packages/graft-sdk/index.d.ts` 是 exact JS contract。
Input camelCase，JSON result 保持 snake_case，如 `expected_head`、`plan_token`、
`state_token`、`materializes_worktree`。Binding 不能私自改 field。

`push`、`fetch`、`pull`、`cloneRepository` 与 `applyMerge` 可接收 `onProgress`
callback。事件报告
当前操作累计的 HTTP body bytes、`upload`/`download` direction，以及 transport 能可靠
提供长度时的 `totalBytes`。总量缺失表示 indeterminate；host 不能用 command phase
伪造 percentage。`applyMerge` 只报告 guarded plan 在 materialize 时仍需 hydration 的
snapshot bytes；若 plan 已完整 hydration，可以不产生 transfer event。

会竞争的 mutation 使用 `expectedHead`、plan token 或 state token。Typed result
包括 status/history/diff/inventory/metadata/merge page、legacy/general `GraftJson`，
或 lifecycle primitive。Batch 要标识 affected path；pagination 要有 continuation；
content 区分 utf8/too-large/missing-payload/invalid-utf8/absent。

## 6. Limit 与 validation

| 类型 | 当前上限 |
| --- | --- |
| history summary | 500 |
| changed/diff path page | 100 |
| explicit diff paths | 10,000 |
| path/merge content | 8 MiB |
| batch mutation | 1,000 |
| inventory | 1,000 |
| ignore query | 1,000 |
| merge paths | 500 |
| merge conflicts / SQLite rows | 1,000 |

JavaScript 缺省为 history/historySummaries 50、changed-path/diff/inventory/merge
list 100、typed SQLite row 100，inventory kind 为 `tracked_ignored`。
`readPathContent`/`readMergeVersion` 必须显式给 `maxBytes`，不会默认无限读取。

Missing required option、invalid union/range/path/revision/identity、互斥 diff mode 必须
在含糊执行前拒绝。除非定义 no-op，empty mutation batch 也拒绝。

## 7. Incremental status cache

SDK 可在 `.graft/cache/sdk-status` 持久化 content-addressed snapshot。当前 schema 3，
最多 4 个，每个 accepted/persisted snapshot 最大 256 MiB。Identity 包括
SDK/repository/object format、HEAD、index、
refs、config、ignore source、tracked 与 visible-untracked fingerprint。返回 hit 前要
复核；任何 mismatch/corruption 全部 invalidates。

写入用 same-directory temp、file sync、atomic rename、directory sync。不能序列化
absolute path 或 credential。Cache 丢失只影响性能。Incremental result 含 generation、
change token、full status 与 cache/stability telemetry。

## 8. Stability 与 materialization gate

SDK 对 file/directory/rename/unlink/symlink race 最多重试三次；仍不稳定返回
`GRAFT_SDK_REPOSITORY_STALE`，不应泄漏 raw platform race error。

`operationMaterializesWorktree` 对以下 operation 返回 true：

```text
Restore, RestorePaths, Pull, Clone, ApplyMerge,
SetMergePathResult, ResolveMergeRow, WriteAndStageTextResult,
ContinueMerge, AbortMerge
```

它表示“可能”写 worktree，不证明本次实际写入。Stage 与 `commit` 明确非物化；旧
config/snapshot/Playground/SDK 文案若写 commit 后重新物化，属于过时文档。

## 9. Cancellation

Rust 使用 cooperative token，Node 接收 `AbortSignal`。Queued work 可直接取消，
started work 在 traversal/diff/history/stage/restore/hydration/SQLite loop checkpoint。
Durable boundary 前取消无效果；multi-path 可能完成合法 prefix，后续 status 可观察。
Remote in-flight publication 可能 outcome unknown。取消不 poison session；JS 映射为
`AbortError`。

### 9.1 Transfer progress

Progress 统计 HTTP upload body 被消费的 bytes，或 HTTP download body 实际收到的
bytes。同一 operation 的多个 request 与 retry 累计计算；已知 request length 加入时
total 随之增长。传输中事件会限频，成功消费完 body 后发送 final event。Progress 只
用于观察，不改变 publication、recovery、cancellation 或 error semantics，也不能把
uncertain remote publication 变成确定结果。

## 10. Stable error

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

JS 除 cancel 外抛 `GraftSdkError(code,cause)`；cancel 为 `AbortError`。Error 必须 redact
credential，并保留有意义 repository/remote conflict detail。

## 11. Node package boundary

Native addon 使用 Node-API 8，async work 离开 JS main thread，共享 Rust session lifetime。
ABI 稳定不等于 binary 跨 OS/arch/libc。CommonJS root package 要求 Node.js 20+，
依次从 `GRAFT_SDK_NATIVE_PATH`、colocated binary 或 exact platform optional package
加载；当前覆盖 macOS arm64/x64、Linux
glibc arm64/x64、Windows x64。没有 browser fallback、install compile 或 remote
binary download。Electron 应在 utility process 使用，并把 `.node` 留在 ASAR 外。

## 12. Browser/WASM profile

```text
React -> message RPC -> Worker -> callMain(graft.wasm)
                              -> WasmFS / OPFS
                              -> memory-backed /tmp
```

OPFS 持久化 repository；`/tmp` memory-backed，避免 temp SQLite DB 泄漏。Host 必须
提供 COOP/COEP，并校验 WASM/source version。当前验证的 Emscripten 是 6.0.3；其他
版本只有在 Wasm feature、WasmFS OPFS、worker startup 与 conformance test 均兼容时
才能替代。Merge UI 调用 real Rust `merge-api`，
不是 pure mock；worker/session 重开后从 durable state reload。

当前 browser 边界：native Node addon 不可加载、remote sync 有意禁用、filesystem
由 OPFS/WasmFS 组合、只支持编译并 bridge 的 graft-cli operation。UI mock 只能测
presentation，不能作为 core conformance。

## 13. Conformance

至少测试 CLI/SDK shared operation 等价、无 PRAGMA control-plane、lifecycle/reopen、
serialization/busy、TypeScript option/result、JSON field、cache、stale retry、cancel、
known/unknown-length HTTP transfer progress、upload-bundle declared total 与 final
event、error/redaction、
materialization gate 与 Node async/package selection。Browser 还要测 real WASM、
OPFS persistence、memory temp、worker restart、merge recovery、version manifest 与
unavailable capability。

当前证据位于 `repo_service.rs`、`graft-sdk`、`graft-sdk-node`、`packages/graft-sdk`、
CLI test 与 `web-demo` worker/unit/Playwright test。

## 14. 当前漂移

- 当前 `docs/sdk-architecture.md` 已记录直接 command-service 边界；本 baseline
  之前仍描述 repository PRAGMA 的旧副本已经过时。
- CLI/SDK 共享实现但 surface 不完全相同；SDK 缺能力不代表另有语义。
- `index.d.ts` 是 JS contract，Rust 私有类型和 CLI human output 不能替代。
- Browser `merge-api` 调用真实 Rust；remote sync 仍不可用，不能用 mock 声称支持。
