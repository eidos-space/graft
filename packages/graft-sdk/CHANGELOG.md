# Graft SDK changelog

This changelog covers the independently versioned `@eidos.space/graft` package. Core Graft CLI and
SQLite extension releases are documented in the repository-level `CHANGELOG.md`.

## Graft SDK 0.3.40 — 2026-11-12

### Changed

- Resolve deletion-versus-edit conflicts when the local parent directory is
  missing. Keeping Local preserves the deletion; choosing Incoming recreates
  the parent and file. Path containment checks still reject outside and
  dangling symlinks.
- Export exact immutable SQLite merge inputs to a new private file with
  `exportMergeSqliteVersion()`. The current state token is required, and the
  output must be outside the repository.
- Add `normalizeMergeSqliteInputs()` for applications that reconcile schema
  identity before comparing records. Graft validates and captures the three
  private inputs, preserves original stages for Undo, and recomputes ordinary
  record conflicts. Applications retain ownership of schema semantics.
- Advance durable merge state after Undo so preparing a semantic provider
  again builds a fresh result seed, including after reopening. Rebuild changed
  indexes without treating the required rebuild as an unsupported row merge.

### 中文

- 修复本地父目录已删除时无法解决“删除与修改”冲突的问题。保留本地继续删除，
  选择对方重建目录与文件；路径检查仍拒绝指向仓库外部或悬空的符号链接。
- 新增带状态令牌校验的 SQLite 原始合并版本导出，输出限定为仓库外的新私有文件。
- 新增应用规范化三份 SQLite 输入的接口，保留原始版本用于撤销，并重新计算记录
  冲突。字段身份与结构变更语义由应用负责，Graft 负责捕获、校验和持久化合并状态。
- 撤销后推进持久状态，重开后再次执行语义合并也会生成新的结果种子；正确重建
  变化的索引，避免将必要的索引重建误判为不支持的记录合并。

## Graft SDK 0.3.39 — 2026-11-03

### Changed

- Initial multipart publication negotiates `e2ee-object-proof-v1` to challenge
  actual stored ciphertext without downloading it. Keep exact multipart spools
  until durable connection, then retire only that initialization's confirmed
  spools. Failed advertised proofs retain recovery state and never downgrade.
- Reconcile retained multipart uploads before publishing or accepting initial
  refs after reopening. Lost completion and proof responses cannot be bypassed
  by object-header discovery; invalid proofs keep initialization pending.
- Older servers and legacy receipts without retained ciphertext use explicit
  `compatibility_verification` progress while fully downloading and verifying.

- First encrypted connection now confirms the pinned repository identity, exact
  encryption config, initial branch and default HEAD after ordered publication,
  without cloning the repository back to a temporary directory. Successful
  `pushInitialBranch()` returns `state: "connected", verification: "publication"`.
- Keep `verificationDirectory` as an explicit independent full restore check;
  success reports `verification: "full_restore"`. Publication confirmation does
  not claim independent recovery or future server durability.
- Report a `confirming` Sync phase after flushing payload progress. Metadata
  reads during this phase do not restart the download indicator or report a
  completed connection before confirmation finishes.
- Resume existing initialization journals with the original source intent and
  remote binding. Unknown publication, changed identity/config/source, missing
  HEAD and cancelled operations do not bypass confirmation or recreate remotes.

### 中文

- 初始 multipart 发布通过 `e2ee-object-proof-v1` 挑战核实实际存储的密文，无需将其下载回来。精确密文保留到连接状态持久化，只清除此初始化意图的已确认文件；声明支持但证明失败时保留恢复状态，不能静默降级。
- 旧服务或已清除密文的旧确认记录仍执行完整认证回读，并显式报告 `compatibility_verification` 阶段。

- 首次加密连接在顺序发布后确认固定身份、精确加密配置、初始分支和 HEAD，
  默认不再完整下载一份临时副本。显式 `verificationDirectory` 仍执行完整恢复检查。
- 新增 `confirming` 阶段；负载进度先完成，元数据确认结束后才报告连接成功。
- 保留原初始化日志和固定意图，支持中断重试；身份、配置或源变化及未确认状态均失败关闭。
  轻量确认不等同于独立恢复验证，也不承诺服务端未来的持久性。

## Graft SDK 0.3.38 — 2026-11-03

### Added

- Plan clone, pull and independent initial-upload verification with a shared
  download inventory for encrypted and plain remotes. Pin the fetched graph,
  deduplicate missing files and SQLite frames, then execute the frozen request
  schedule. Encrypted plans authenticate object headers and count actual
  ciphertext ranges, including repeated headers and AEAD covering blocks.
- Add `fetchForCheckout` for hosts that review before applying a pull. It prepares
  target files and SQLite pages under one download budget and updates the
  tracking ref without changing HEAD, the index or the worktree. Follow with
  `planMerge` and guarded `applyMerge`.
- Plan multipart uploads before transferring missing parts, retaining validated
  session negotiation, original encrypted retry bytes and full-object readback.
  Plain aggregate and per-object fallback uploads use the same cumulative body
  accounting as encrypted transfers.

### Boundaries and compatibility

- `onProgress` reports cumulative body bytes and a fixed total only when the
  complete transfer schedule is known. `onSyncProgress` request totals remain
  request-local. Upload and independent verification have separate budgets;
  reaching a transfer total does not mean that verification, ref publication,
  merge application or the SDK operation has completed.
- Encrypted upload inventories retain at most 2 GiB/256 payloads and 1 MiB/256
  public hints. Exceeding a planning bound falls back to validated streaming
  with real cumulative bytes and an unknown total. Unexpected ranges, retries
  and short bodies withdraw a declared total. Temporary preparation storage and
  preflight requests add overhead; exact percentages are not guaranteed for
  every operation. Ref/hint control traffic and HTTP/TLS framing are excluded.
- Existing authentication, epoch checks, ref CAS, multipart recovery and guarded
  worktree updates remain required. Repository formats and remote service
  protocols are unchanged; existing services need no upgrade for these counters.
- Native packages cover macOS arm64/x64, Linux glibc arm64/x64 and Windows x64
  on Node.js 20 and newer. Release checks exercise Node.js 20 and 24. Linux musl
  is not included. A previously reproduced Linux root-user WAL status/ctime
  failure also affects 0.3.37 and is not repaired by this progress release.

### 中文

- 补齐加密与非加密 clone、pull 和首次上传独立校验的下载规划：固定远端图，
  去重缺失文件与 SQLite 帧，冻结实际请求调度；加密按已认证的密文区间计量。
- 新增 `fetchForCheckout`，在同一下载预算内准备目标文件和数据库页，只更新
  tracking ref，不改 HEAD、index 或工作目录；随后仍须审核并受保护地应用合并。
- multipart 先协商并排除已确认分片，再统计缺失分片；保留原始密文重试、恢复及
  完整读回验证。非加密 aggregate/fallback 与加密路径统一累计实际 body 字节。
- 完整规划时才提供固定总量；上传与独立校验分开计量，传输达到 100% 不代表整个
  操作成功。超过 2 GiB/256 payload 或 1 MiB/256 hint 等规划预算时显示实际累计
  字节和未知总量；计划外范围、重试或短读会撤销分母。规划有预检和暂存开销。
- 无仓库格式或服务端协议变更。支持原五个平台及 Node.js 20+，不含 Linux musl；
  已知 Linux root 用户 WAL 状态/ctime 问题在 0.3.37 同样存在，本版未修复。

## Graft SDK 0.3.37 — 2026-11-02

### Added

- Add optional `onSyncProgress` stages and request-local HTTP body counters to
  push, fetch, pull, clone and encrypted initialization. Existing `onProgress`
  retains cumulative transfer bytes; request percentages never imply operation
  completion. Cancellation and settlement suppress late sync events.
- When `onSyncProgress` is supplied, encrypted initial uploads and daily pushes
  can prepare a complete, bounded ciphertext inventory before publication and
  report its fixed upload payload total through `onProgress`. The inventory
  includes SQLite segments, commits, external files, packs and indexes; existing
  authenticated discovery still avoids known remote payloads.

### Boundaries and compatibility

- Encrypted clone/pull and initial upload's independent verification report real
  bytes and stages with an unknown operation total. Multipart and inventories
  exceeding 2 GiB/256 payloads retain the existing validated streaming path and
  unknown total. Public hint retention is limited to 1 MiB/256 hints. Ref/hint
  control traffic and HTTP/TLS framing are excluded from the upload payload total.
- Planning retains prepared ciphertext files and changes aggregate publication
  to individual immutable payload requests. Its preparation time and retained
  bytes are observable with `GRAFT_PUSH_TRACE=1`. Legacy transfer-only observers
  keep streaming. A short body or internal retry withdraws a declared total.
- Immutable-object authentication, epoch validation, ref CAS, multipart recovery,
  and independent initial-publication verification remain required. Repository
  formats and server protocols are unchanged.
- Preserve macOS Mach-O symbol tables when building addons to avoid a dyld
  LINKEDIT alignment error; re-sign the final copied artifact.
- Native packages cover macOS arm64/x64, Linux glibc arm64/x64 and Windows x64.
  The release matrix tests Node.js 20 and 24; Linux musl is not provided.

### 中文

- 新增可选 `onSyncProgress`，提供首次上传、clone、push、fetch、pull 的实际阶段与单次 HTTP 请求字节；已有 `onProgress` 继续提供累计字节，不把请求百分比当成操作完成度。
- 加密首次上传和日常 push 在完整密文清单可规划时，通过 `onProgress` 提供固定上传主体总量；包含 SQLite、提交、附件、pack 和索引。须同时提供 `onSyncProgress` 才启用规划。
- 加密 clone/pull、首次上传后的独立校验、multipart 及超过 2 GiB/256 个主体的操作仍保持未知总量；提示数据最多保留 1 MiB/256 条。总量不包含 ref/hint 控制请求及 HTTP/TLS 开销。
- 规划需暂存完整密文，聚合发布改为逐个不可变对象请求；可通过 `GRAFT_PUSH_TRACE=1` 查看准备耗时与暂存字节。旧的仅传输回调继续流式发送；短读或内部重试撤销已声明总量。
- 保留认证、密钥会话、引用 CAS、multipart 恢复及独立校验，仓库格式和服务协议不变。修复 macOS 原生库的 LINKEDIT 对齐加载问题。

## Graft SDK 0.3.36 — 2026-11-01

### Fixed

- Recompute storage commit identities when restoring a SQLite snapshot into a
  new log. Restored checkpoints can be published and cloned by another device
  without a snapshot storage commit hash mismatch.
- Keep offline edits of lazy SQLite checkouts usable when superseded historical
  pages have not been downloaded. Seed a fresh checkpoint from visible pages
  instead of rebinding incomplete storage history; existing versions remain intact.

### Compatibility

- Public SDK APIs, repository formats, and remote protocols are unchanged.
- This prevents new inconsistent checkpoints. It does not automatically rewrite
  previously affected repository history.
- Native packages support macOS arm64/x64, Linux glibc arm64/x64, and Windows x64
  on Node.js 20 and newer.

### 中文

- 修复恢复 SQLite 快照到新存储日志时沿用旧提交哈希的问题，恢复后的检查点可正常发布并由其他设备克隆。
- 惰性下载的 SQLite 工作副本缺少被覆盖的历史页时，仍可离线编辑；使用当前可见页建立新检查点，保留已有版本，不依赖额外下载旧历史页。
- 公共 API、仓库格式和远端协议不变；此修复防止生成新的异常检查点，不会自动改写已有的受影响历史。
- 继续支持 macOS arm64/x64、Linux glibc arm64/x64、Windows x64 和 Node.js 20 及以上版本。

## Graft SDK 0.3.35 — 2026-10-22

### Fixed

- Reject status-cache updates when data files change during status collection, so
  edits cannot be persisted as a clean worktree under newer file fingerprints.
- Rebuild older status snapshots that could contain this incorrect clean
  classification. Local files, commits, branches, tags and remotes are preserved.

### Compatibility

- Public SDK APIs, repository formats, and remote protocols are unchanged.
- Status checks continue to support open SQLite databases in WAL mode. Reader
  coordination may update SHM timestamps; database, WAL and other data-file
  fingerprints still have to remain stable throughout collection.
- Native packages retain support for macOS arm64/x64, Linux glibc arm64/x64,
  and Windows x64 on Node.js 20 and newer.

### 中文

- 修复数据文件在状态采集期间发生变化时，修改可能被持久缓存为“干净”的问题；采集前后的数据文件指纹必须一致。
- 自动重算旧版状态缓存，保留本地文件、提交、分支、标签和远程配置。
- 继续支持保持打开的 WAL 模式 SQLite 数据库，允许读者更新共享内存协调时间戳，同时严格检测数据库、WAL 和其他数据文件的变化。
- 公共 API、仓库格式和远端协议不变；继续支持 macOS arm64/x64、Linux glibc arm64/x64、Windows x64 和 Node.js 20 及以上版本。

## Graft SDK 0.3.34 — 2026-10-19

### Fixed

- Republish complete SQLite history from a lazy clone to a new remote. Missing
  historical snapshot data is hydrated from an eligible original source before
  publication, so another device can read the published old commits.
- Reuse live keyspace-ID mappings during journal recovery to avoid repeated
  metadata lookups while preserving deleted and recreated keyspaces.

### Compatibility

- Public SDK APIs, repository formats, and remote protocols are unchanged.
- Native packages retain support for macOS arm64/x64, Linux glibc arm64/x64,
  and Windows x64 on Node.js 20 and newer.

### 中文

- 修复惰性克隆后向新远端发布 SQLite 历史时缺失旧快照数据的问题：从原来源补齐后再发布，确保另一设备能读取历史提交。
- 日志恢复复用有效 keyspace ID 映射，减少元数据查询，并保持删除后重建 keyspace 的行为。
- 公共 API、仓库格式和远端协议不变；继续支持 macOS arm64/x64、Linux glibc arm64/x64、Windows x64 和 Node.js 20 及以上版本。

## Graft SDK 0.3.33 — 2026-10-18

### Fixed

- Report actual encrypted HTTP upload and download bytes, including aggregate
  bundles and multipart uploads, through the existing transfer callback.
- Forward `onProgress` from `cloneEncryptedRemote` and `pushInitialBranch`.
  Initial publication also reports its independent verification download.
- Keep encrypted transfers indeterminate instead of treating plaintext checkout
  budgets as final ciphertext totals. Hosts can show byte counts and speed
  without an inaccurate percentage or remaining-time estimate.
- Preserve cancellation and retry semantics; each new operation starts fresh
  counters and callbacks drain before its promise settles.

### 中文

- 加密上传和下载现在报告真实传输字节数，覆盖聚合传输和分片上传。
- `cloneEncryptedRemote` 和 `pushInitialBranch` 支持 `onProgress`；首次上传的独立回读验证也会报告下载进度。
- 加密传输不再将明文预算当成最终密文总量，宿主可以显示字节数和速度，避免错误百分比或剩余时间。
- 保留取消和重试语义；每次新操作重新计数，并在 Promise 完成前排空进度回调。

## Graft SDK 0.3.32 — 2026-10-18

### Added

- Add idempotent `RepositorySession.removeRemote(name)` for disconnecting a named
  remote while preserving local files, unsaved edits, commits, branches and tags.
- Enable encrypted synchronization and migration on Windows, with protected
  private directories, durable replacement and native file identity checks.
- Add `EncryptedRemoteSession.createPrivateDirectory(path)` for native private
  host storage on Unix and Windows.

### Fixed

- Require retained initial branch/commit intent before source publication,
  and durable recovery journals before encrypted ref mutations.
- Bound encrypted transfers by a finite total deadline as well as read inactivity
  timeouts. Preserve uncertain publication state for reconciliation.
- Fence and drain all sessions sharing a remembered-key identity during removal.
- Harden private staging, recovery replacement and password input buffers.

### Compatibility

- Hosts publishing an initial branch must pass `{ branch, commit }` to
  `initializeConfig(password, source)` before configuration publication. A later
  HEAD must never substitute for missing retained intent.
- Windows hosts must create checkpoint, recovery and staging directories with
  `EncryptedRemoteSession.createPrivateDirectory`. Windows sessions use explicit
  passwords; no built-in Windows persistent key store is provided.

### 中文

- 新增 `RepositorySession.removeRemote(name)`，移除指定远程连接及其跟踪配置，保留本地文件、未保存改动与 Graft 历史。
- Windows 支持加密同步与迁移，使用受保护的私有目录、持久替换及原生文件身份检查。
- 新增 `EncryptedRemoteSession.createPrivateDirectory(path)`，用于创建满足 Unix / Windows 权限要求的宿主状态目录。
- 首次源分支发布必须使用预先保留的分支和提交意图，加密引用修改必须有持久恢复日志；传输同时受总期限和读取超时限制，结果未知时保留恢复状态。
- 移除记住的密钥时会阻止并排空所有共享同一身份的会话；加固私有暂存、恢复替换和密码输入缓冲区。
- 发布首次分支的宿主需在配置发布前调用 `initializeConfig(password, { branch, commit })`，不能从后来变化的 HEAD 补建意图。Windows 宿主需通过新目录 API 创建私有状态；Windows 目前使用显式密码，不提供内置持久密钥存储。

## Graft SDK 0.3.31 — 2026-10-17

### Added

- Trusted Node hosts can use `EncryptedRemoteSession` for password-protected encrypted remotes. The native authority binds account identity and repository UUID, keeps root keys outside JavaScript, and supports unlock, lock, pause, token refresh and disconnect.
- Encrypted initialization, initial branch publication and clone support independent read-back verification. Durable private checkpoints and recovery journals retain uncertain publication bytes across process restarts.
- Explicit history migration supports selected refs, independent verification and a native repository mutation fence for local cutover. Password changes retain their transaction for reconciliation after interruption.
- Official native packages include encrypted-session APIs. macOS packages include the optional native Keychain provider; Linux packages support private owner-only filesystem state. The release gates test each packaged addon on Node.js 20 and 24 and exercise real encrypted sync and migration on the four Unix targets.

### Compatibility

- Existing plain repositories remain supported. Encrypted remote creation and migration require an encrypted-capable service and explicit host enrollment; updating the SDK does not migrate an existing remote automatically.
- Windows native packages export the API but reject encrypted enrollment with `GRAFT_SDK_E2EE_UNSUPPORTED_ENCRYPTED_HOST`. Windows encrypted synchronization is not supported in this release.
- Hosts must retain private checkpoint and recovery directories outside the worktree, coordinate their own file/SQLite writers, and verify the account identity before token refresh. Password reset and revocation of offline devices are not provided. Keychain remembering requires a correctly signed and entitled macOS host.

### 中文

- 新增面向可信 Node 宿主的 `EncryptedRemoteSession`，支持密码解锁、锁定、暂停、令牌刷新和断开连接。原生会话绑定账户身份和仓库 UUID，根密钥不进入 JavaScript。
- 加密仓库初始化、首次分支上传和克隆支持独立回读验证。私有检查点和恢复日志保留结果未确认的发布数据，使进程重启后能够继续核对和恢复。
- 历史迁移支持显式选择引用、独立验证，以及由原生仓库写入锁保护的本地切换；修改密码也保留可恢复的事务记录。
- 正式原生包包含加密会话 API。macOS 包包含可选的系统钥匙串支持，Linux 使用仅当前用户可访问的私有目录。Windows 包会明确拒绝加密注册，本版不支持 Windows 加密同步。
- 普通仓库继续兼容，升级不会自动迁移已有连接。宿主仍负责账户校验、私有状态保存及普通文件/SQLite 写入协调；不提供忘记密码重置或离线设备撤销。使用钥匙串记住密码要求 macOS 宿主具备正确签名和所需权限。

## Graft SDK 0.3.30 — 2026-10-09

### Improved

- Active merge status and repeated conflict queries reuse session-local results
  after checking repository metadata, worktree fingerprints and SQLite sidecars.
  Conflict analysis is bounded to 4 MiB and invalidates after external writes.
- Ancestry and reachability queries read verified commit parent edges without
  loading historical file trees or computing unrelated diffs.
- Merge-base reuse follows the actual active merge heads, including peer merges
  whose heads differ from the configured upstream.
- Frozen merge-policy reads and resolution-journal updates read their durable
  heads and journal directly, avoiding unrelated worktree and history scans.

### Compatibility

- The SDK API, repository format and Remote protocol are unchanged. No data
  migration is required. Merge-policy integrity checks and final merge
  validation remain enabled. Application-specific automatic merge rules remain
  the host application's responsibility.

## Graft SDK 0.3.29 — 2026-10-07

### Added

- Rust sessions can use `fetch_for_checkout` to download and validate a target
  revision before applying it to the worktree, with one planned transfer budget
  for external files and missing compressed SQLite segments. Cached data is
  excluded and shared content is counted once.
- Transfer progress exposes `total_is_final` in Rust and `totalIsFinal` in Node.js
  so hosts can distinguish a planned total from a running sum of response sizes.

### Fixed

- Android HTTP remotes use the platform DNS resolver instead of requiring Unix
  resolver configuration.
- Repository HTTP connections keep running between synchronous calls and
  executors. Parallel probes release excess idle connections for later uploads.
- Cancelled or invalid prepared fetches do not advance the remote tracking ref.

### Improved

- Fetch preloads small external payloads with four bounded concurrent requests,
  retaining content verification and the fallback for larger files.
- SQLite status reuses exact-state probes while detecting external and WAL writes.
- Fast-forward merges preserve unchanged SQLite files and apply only changed paths.

### Compatibility

- Repository, snapshot, and Remote protocol formats are unchanged. No data
  migration is required. Rust consumers constructing `TransferProgress` directly
  must supply the new `total_is_final` field.
- Fast-forward merge results list only changed paths in `worktree_paths`.

## Graft SDK 0.3.28 — 2026-09-29

### Fixed

- Restoring a SQLite file to a revision where it was deleted now reports the
  deletion correctly. Staging and committing no longer resurrect the file from
  a cached snapshot, and another device receives the deletion when syncing.

### Compatibility

- The SDK API, repository format and Remote protocol are unchanged. No data
  migration is required.

## Graft SDK 0.3.27 — 2026-09-08

### Improved

- Avoid repeated ancestry scans when upstream tips are unchanged, while invalidating cached projections when refs change.
- Deduplicate immutable object validation within a single fetch, while revalidating on subsequent fetches.
- Read inactive merge policy without a full worktree status scan.

## Graft SDK 0.3.26 — 2026-09-07

### Added

- Add `pathHistory` for exact-path, first-parent history with a HEAD-pinned cursor,
  independent comparison/metadata-byte budgets, cancellation, and actual read telemetry.
  Renames are intentionally not followed; deletions and recreation remain visible.
- Document and test worktree-only `restorePaths`: HEAD and index stay unchanged,
  unrelated dirty files are preserved, and external target edits require host coordination.
- Add a reproducible native history/restore benchmark and an unpublished host-package
  assembler (`scripts/pack-local.mjs`) for integration testing.

## Graft SDK 0.3.25 — 2026-08-28

### Fixed

- Push after a merge now excludes the complete history already reachable from the Remote head,
  preventing previously published commits and snapshots from being uploaded again.
- HTTP push falls back to individual object transfer when a receive bundle would exceed the
  service's object-count or manifest-size limits, avoiding a terminal `413 Payload Too Large`
  failure for repositories with longer histories.

### Compatibility

- The JavaScript API, repository format, snapshot format, and Remote protocol are unchanged. No
  migration or application code change is required.

## Graft SDK 0.3.24 — 2026-08-28

### Added

- Rust and Node.js repository sessions can read, list, set, and unset repository configuration,
  including the Git-style `user.name` and `user.email` identity keys.
- Session construction accepts an in-memory identity override. The override applies to commits,
  annotated tags, and reflogs for that session without changing `.graft/config.toml`.

### Fixed

- Persistent incremental-status loading now skips stale candidates and continues searching for an
  exact worktree fingerprint match, avoiding reopen cache misses when snapshot mtimes tie or sort
  differently across platforms.

### Compatibility

- The new Rust and JavaScript APIs are additive. Repository and snapshot formats are unchanged,
  and sessions without an identity override retain repository configuration and the deterministic
  fallback identity.

## Graft SDK 0.3.22 — 2026-08-27

### Fixed

- SQLite worktree comparisons now ignore the schema cookie rewritten by online backup, preventing
  clean databases committed on macOS from appearing modified after a Windows clone.

### Compatibility

- The JavaScript API, repository, and snapshot formats are unchanged. Disposable SQLite page-index
  and worktree-probe caches are rebuilt under version 5.

## Graft SDK 0.3.21 — 2026-08-26

### Changed

- Owner-published incremental status snapshots can now be revalidated without opening runtime
  storage, allowing the matching Graft CLI to serve exact `status` results while an Electron or
  Node.js application retains the repository lock.
- Repository index updates now use atomic replacement so lock-independent readers observe a
  complete old or new index.

### Compatibility

- The JavaScript API and repository/status-cache formats are unchanged. Existing SDK clients do
  not require code changes.

## Graft SDK 0.3.20 — 2026-08-25

### Fixed

- Incremental SQLite capture deltas now apply reliably to exported WAL-mode base snapshots because
  Graft preserves their exact page-1 bytes and SHA-256 identity.

## Graft SDK 0.3.19 — 2026-08-25

### Added

- `captureSqliteSnapshot()` exports a consistent create-new SQLite image without changing Graft
  history or the worktree. A prior opaque capture token enables page reuse and an optional bounded
  `GRAFTD01` fixed-page delta for content-addressed publication transports. Capture results now
  expose the exact snapshot SHA-256; delta results expose their embedded base and target digests.

## Graft SDK 0.3.18 — 2026-08-17

### Changed

- Fetch records commit ancestry in new object-pack indexes and uses it to prefetch the complete
  pack chain from the requested head to a local ancestor in one bounded read bundle. The retained
  eight-push plus unrelated-branch regression collapses the cold Fetch topology from 19 HTTP
  requests to 4 while excluding the unrelated pack. Pre-existing indexes receive a bounded
  128-pack/48 MiB migration path before falling back to exact lazy reads.
- Fetch coalesces immutable pack-index reads and missing SQLite snapshot commits into bounded
  Remote read bundles. In the retained Windows trace with 15 pack indexes, the index phase changes
  from 15 authenticated HTTP GETs to one request; longer histories are split automatically at the
  service limit.
- A completed fast-forward pull no longer performs an immediate full worktree status scan merely
  to confirm that no merge remains. The next explicit status request remains authoritative and
  still detects concurrent external writes.
- Pack indexes are cached locally as repairable performance hints, so unchanged immutable indexes
  are not downloaded again after reopening the application.
- macOS and Windows row auto-merge can seed the first private candidate from the Ours worktree
  instead of reconstructing the complete SQLite file from page storage. The private copy must
  match Ours' exact content-addressed page index before use; a missing index, changed worktree, or
  copy mismatch retains authoritative snapshot materialization.
- Verified candidate construction and the complete immutable-Ours integrity check now overlap row
  merge analysis. Unused candidates are cancelled and removed, while a required candidate is not
  published until exact page-index verification and the full check both succeed.
- New local storage commits retain the hash computed from their already-resident segment pages;
  legacy commits are backfilled after their first hash calculation. Sparse merge imports no longer
  rescan a historical full-database segment to rebuild hashes that were already established.
- SQLite merge planning reuses one dense page-origin manifest per Base/Ours/Theirs snapshot across
  candidate selection and parallel row diffs instead of rebuilding ordered maps for each pass.

### Compatibility

- The optional `commits` field in pack-index format version 1 is an integrity-neutral hint. Older
  clients ignore it, while new clients validate fetched object bytes exactly as before.
- Repository and snapshot formats are unchanged. SDK clients fall back to bounded individual reads
  against Remote services that do not advertise or accept `read-bundle`.

## Graft SDK 0.3.17 — 2026-08-17

### Fixed

- Merge completion now resolves and verifies every staged SQLite snapshot from the configured
  Remote before recording the two-parent commit, preventing an incomplete local snapshot from
  becoming history that cannot be pushed.
- Push preflight now hydrates referenced storage commits from the destination Remote when the local
  repository no longer has them. Valid Remote-backed history can recover and publish instead of
  entering a repeated missing-storage failure loop.

### Compatibility

- Repository, snapshot, merge, and Remote formats are unchanged. If the referenced storage is
  absent both locally and remotely, the operation still fails without publishing partial history.

## Graft SDK 0.3.16 — 2026-08-16

### Fixed

- `applyMerge()` now accepts the same optional transfer-progress callback as fetch and pull. Hosts
  can fetch once, plan against the Remote-tracking ref, and display any snapshot hydration bytes
  while atomically applying that immutable plan without a redundant Remote-ref fetch.

## Graft SDK 0.3.15 — 2026-08-15

### Added

- `repositoryMetadata()` now reports the last locally known upstream target so history UIs can
  decorate a diverged Remote tip without fetching or scanning the worktree.

### Changed

- SQLite merge preparation reuses proven segment page sets across immutable snapshots and targets
  `WITHOUT ROWID` analysis to changed B-tree pages instead of repeatedly materializing complete
  database pairs. Candidate construction also establishes one full integrity proof for an exact
  immutable Ours state before mutation, then validates SQLite's transactional delta instead of
  cold-scanning every inherited page again. On the retained macOS 417 MiB Eidos fixture, warmed
  merge lifecycle P95 fell from 42.34 seconds to 1.56 seconds; a fresh process completed in 2.93
  seconds.
- On macOS, an already-proven, clean, exclusively locked Ours worktree seeds private merge
  candidates with an APFS copy-on-write clone. Unsupported filesystems and other platforms retain
  the authoritative snapshot-materialization fallback.
- Transactional SQLite merge replay now carries committed WAL page numbers into repository import,
  so validated sparse changes avoid rereading the complete candidate. Missing, malformed, or
  partial WAL data retains the authoritative full-import fallback.
- A validated final SQLite merge candidate is installed directly and exact-token
  `continueMerge()` commits that staged state without a second database rewrite. When completion
  does not physically change a path, its exact `worktree_paths` result is empty. The SDK also
  carries validated file fingerprints across that ref/index-only commit, avoiding an immediate
  full status scan while preserving stat-based invalidation for external writes.

### Fixed

- Multi-request pushes now declare their complete known upload payload before transfer starts.
  Progress no longer reports each completed request as a misleading new `100%` total; fallback
  retries add their remaining payload as one planned unit.
- Progress callbacks are rate-limited across short request bodies, and the JavaScript operation
  waits one event-loop turn before settling so hosts receive the exact final byte count without a
  large callback backlog making a completed push appear stuck.

### Compatibility

- `upstream_target` is additive and nullable. Repository, snapshot, merge, and Remote formats are
  unchanged. `operationMaterializesWorktree("continueMerge")` remains a conservative `true` host
  gate even when the validated completion fast path returns no changed worktree paths.

## Graft SDK 0.3.14 — 2026-08-13

### Changed

- Clone transfer progress now uses the exact framed upload-bundle size declared by newer Graft
  Remote services, allowing hosts to show total download size, percentage, and estimated time
  remaining from the start of the streamed response.

### Compatibility

- Older Remote services remain supported through their HTTP `Content-Length`. New clients reject a
  response only when the Graft total-size header and `Content-Length` are both present but disagree.

## Graft SDK 0.3.13 — 2026-08-13

### Added

- Added a generic, durable semantic-merge provider handoff for application-owned SQLite rules.
  Providers receive immutable Base/Ours/Theirs snapshots plus a private Ours-derived candidate
  containing safe non-managed Theirs changes, and may persist bounded domain conflicts or accept a
  validated result under exact provider and merge-state tokens.

### Fixed

- Semantic result acceptance now records its validation audit before worktree materialization, so a
  process interruption remains retryable instead of leaving an apparently failed but already staged
  path.

### Compatibility

- Repository, snapshot, merge-journal, and Remote formats are unchanged. The provider APIs are
  additive, provider workspaces stay private under `.graft`, and Node.js 20 remains the minimum
  supported runtime.

## Graft SDK 0.3.12 — 2026-08-13

### Changed

- Row, cell, and table conflict choices now update the durable resolution journal without
  rebuilding the SQLite candidate until the path is complete, and retained sessions reuse the
  immutable merge plan between choices.
- Merge operations report only the paths actually materialized, allowing hosts to skip redundant
  validation and reopen work after journal-only choices.

### Fixed

- Resolving SQLite conflicts entirely to the existing Ours result now reuses the parent snapshot
  instead of recording a physical-only merge modification with no schema or row changes.

### Compatibility

- Existing repository, snapshot, merge-journal, and Remote formats are unchanged. The new
  `worktree_paths` fields are additive, and Node.js 20 remains the minimum supported runtime.

## Graft SDK 0.3.11 — 2026-08-13

### Added

- Added real HTTP upload/download byte progress for `push`, `fetch`, `pull`, and
  `cloneRepository`, including optional totals and rate-limited final events for indeterminate
  response bodies.

### Fixed

- Freshly cloned SQLite worktrees no longer appear modified when the source and destination were
  written by different SQLite library versions; volatile page-one header metadata is ignored while
  actual database-content changes remain detectable.

### Compatibility

- Existing repository and Remote formats are unchanged. Transfer progress callbacks are optional
  and additive, and Node.js 20 remains the minimum supported runtime.

## Graft SDK 0.3.10 — 2026-08-12

### Added

- Added versioned, compare-and-swap merge policies with explicit same-row merging, semantic-key
  collations, and data-only managed-column resolvers. Plans bind the exact policy token and active
  merges freeze that policy until continue or abort.
- Added durable cell and safe table resolution, reversible path resolution, and validated staging
  of an application-owned SQLite result. Every new asynchronous SDK operation accepts an
  `AbortSignal`.
- Added directory-wide multi-SQLite schema union, validated table rebuilds, SQLite internal-state
  resolvers, and structured diagnostics for skipped, corrupt, or analysis-failed tracked paths.

### Changed

- Merge inspection now retains resolved and unresolved conflict detail across session reopen and
  reports structured cell values, resolution state, recommendations, and current policy metadata.
- Worktree materialization declarations now cover every merge apply, resolution, unresolve,
  edited-text, continue, and abort operation so hosts can close and restore live SQLite handles.

### Fixed

- Compatible column, table, index, view, and trigger changes now form one validated result across
  every SQLite database in a directory-backed repository.
- Complete-file table/view collision choices no longer read the wrong object as a table B-tree, and
  malformed tracked SQLite files remain dirty with a precise non-destructive diagnostic.

### Compatibility

- Existing repository and Remote object formats are unchanged. Policy v1 defaults
  `same_row_merge` to false, so applications must opt in explicitly for formats that support it.
- Policy resolvers remain generic and data-only; application-specific recomputation and domain
  validation stay with the host before `stageMergeSqliteResult`.
- Node.js 20 remains the minimum supported runtime.

## Graft SDK 0.3.9 — 2026-08-11

### Added

- Added a durable merge workflow spanning `fetch`, `planMerge`, `applyMerge`, merge status and path
  inspection, bounded base/ours/theirs/result reads, text editing, whole-path selection, SQLite row
  resolution, `continueMerge`, and `abortMerge`.
- Added immutable plan and state tokens so host applications can reject stale merge plans, changed
  HEADs, concurrent writers, and outdated conflict resolutions without overwriting either side.
- Added durable conflict recovery across `close`, `open`, and replacement utility processes, with
  active merge state reconstructed from repository refs and index stages.

### Changed

- Expanded the Node.js declarations and wrapper contract to expose up-to-date, fast-forward, and
  three-way merge outcomes with typed conflict details and materialization gates.
- Documented when applications must close and reopen long-lived SQLite handles around operations
  that may replace physical worktree files.

### Fixed

- Snapshot preparation now validates hydration markers and fetches the exact missing LSN range,
  repairing incomplete local state without re-fetching already available data.

### Compatibility

- Existing repository and Remote object formats are unchanged, and all pre-0.3.9 SDK methods retain
  their signatures. The merge surface is additive.
- Node.js 20 remains the minimum supported runtime; application-database integration tests using
  built-in `node:sqlite` run on Node.js 22 or newer.
