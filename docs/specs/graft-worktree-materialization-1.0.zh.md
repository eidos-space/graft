# Graft Worktree Materialization 1.0

状态：与实现对齐的规范草案
版本：1.0
发布日期：2026-08-11
规范语言：英文（本文件为中文信息性参考）

## 摘要

Graft 对应用 worktree 进行版本管理。SQLite 数据库以 canonical snapshot
的形式存在于 repository index 和 commit 中；默认 host 模型还会把这些
snapshot 投影为 repository-relative 路径下的普通物理 SQLite 文件，使已有
SQLite 应用可以继续使用普通 connection。

本规格定义 Graft 何时可以创建、替换或删除这些物理文件，如何处理 WAL 与
应用 connection，CLI、Rust SDK、Node SDK、浏览器/WASM bridge 如何映射到
同一行为，以及如何区分以下几类动作：

- physical worktree materialization；
- snapshot/payload hydration；
- row-diff 的临时 `materialized_compat` 数据库；
- export；
- 应用或 `graft sql` 直接创建/修改普通 SQLite 文件。

本规格同时区分“保守 gate”和“实际效果”：
`operationMaterializesWorktree` 的含义是**该操作可能改写 worktree**，不是
“本次调用必然发生了改写”。

## 文档状态

英文文档中的大写关键字具有规范性。当前版本以仓库现有源码和测试为依据，
不把当前物理文件流程描述成跨路径事务：单路径 replacement 是原子的，多路径
checkout 在后续路径失败时仍需要恢复和 reconcile。已知缺口见第 12 节。

## 1. 范围、边界与 conformance

职责链如下：

```text
CLI / Rust SDK / Node SDK / WASM bridge
                 |
                 v
RepositoryCommandService 与 RepositorySession
                 |
                 v
Graft core：refs、index、snapshots、merge state
                 |
                 v
SQLite worktree adapter：物理文件、WAL、锁、replacement
```

Graft core 负责 canonical snapshot、commit/path identity、refs、index stages、
merge topology、durable merge state 和 stale-head 检查；它本身不决定普通
SQLite 路径是否写回磁盘。

SQLite worktree adapter 负责物理 SQLite 检查、一致 WAL 捕获、replacement guard、
临时 checkout 文件、sidecar 清理、volume binding 与文件系统恢复；它不能
重新定义 commit 或 merge 的语义。

CLI 和 SDK 负责参数校验、操作命名、session 串行化、错误映射和结果投影。
Playground 只负责展示，使用 browser profile；不能再实现一套 merge 或
materialization 算法。

实现若声明 `GWM-Reader-1.0`，必须遵守非物化保证以及第 2 节的几类动作分离。
声明 `GWM-Writer-1.0`，还必须满足第 6、10 节的 replacement、WAL、handle 和
恢复要求。CLI、SDK、Browser profile 都是这些 core 要求的映射。

## 2. 术语与不可互换的操作

### 2.1 Physical worktree materialization

**Physical worktree materialization** 指 Graft 因为改变 checkout 状态或解决
merge，而在 worktree 中创建、替换或删除 tracked file。对 tracked SQLite
路径而言，它表示在 repository-relative 路径写出一个独立的普通 SQLite 数据库，
并在需要时更新与该路径对应的 Graft volume binding。

本规格中的“materialized”绝不单纯表示字节被读入内存，或 Graft volume 被打开。

### 2.2 Canonical snapshot 与 snapshot/payload hydration

**Canonical SQLite snapshot** 是数据库的 staged 或 committed Graft 表示，包含
page 内容、page metadata 和 repository path identity。staged snapshot 是
`commit` 的 source of truth；它不会在 commit 时重新从可能已变化的 worktree
读取。

**Snapshot hydration** 是为检查或应用 snapshot，把 snapshot page、object 或
external payload 解析/加载到 Graft 的 local runtime/storage。它可以使用本地
或远程 store，也可以只加载请求范围。它不得因此创建、替换或删除应用
worktree 文件，也不得报告为 `materializes_worktree`。

例子包括 merge plan/apply 时加载远程 target、`payload fetch`、audit repair，
以及加载历史 commit blob 做 diff。

### 2.3 `materialized_compat`

`materialized_compat` 是 row-diff 在不能直接检查 page/table 时使用的临时
SQLite compatibility database，例如 page size 不是 native 值，或表使用
`WITHOUT ROWID` 需要普通 SQLite 执行时。它是内部 temporary pair，不是
repository worktree 路径。它必须作为 diff response scope 报告，不能报告为
物理 worktree materialization。

### 2.4 Export

`graft export` 把当前 worktree 或指定 revision 写到 caller 选定的另一个普通
SQLite 文件。它不会移动 `HEAD`、更新 index、更新 tracked path binding，也
不算 checkout materialization。

如果 caller 故意把 tracked worktree 路径指定为 export destination，那是 caller
直接执行的外部文件覆盖，不能利用 export 的 non-materializing 分类绕过 handle
和 recovery 要求。

### 2.5 直接物理 SQLite 访问

应用、`sqlite3` 或 `graft sql` 可以直接创建或修改普通 SQLite 文件。这是
external worktree edit，不是 Graft materialization。之后 `graft add` 捕获该文件
的一致 snapshot（包括已提交的 WAL frames）并 staging。connection 和 transaction
生命周期由应用负责。

## 3. 默认物理 SQLite 模型

### 3.1 Worktree 与 canonical identity

默认配置为：

```toml
[worktree]
materialize_sqlite = true
```

`path/to/data.sqlite` 通过 repository-relative path 被跟踪。commit 保存的是
canonical snapshot descriptor 与 path identity，不承诺 checkout 后物理 inode
仍然不变。若 host 要在不重新读取 payload 的情况下保留 tracked path identity，
必须通过显式 SDK `recordPathMove` 表达物理 rename。

只有在 Graft 捕获到一致 SQLite image 后，当前物理文件才是 staging 的输入；之后
staged canonical snapshot 成为下一次 commit、历史 diff、merge plan 和 checkout
的依据。

### 3.2 配置语义

`worktree.materialize_sqlite = true` 允许 checkout 类操作把 tracked SQLite
snapshot 写回 repository-relative 物理路径。这是默认模式，也是普通 SQLite
工具的兼容模式。

`worktree.materialize_sqlite = false` 会关闭 checkout plan 的普通 SQLite 投影。
repository 仍然更新 canonical state 与 Graft volume/path binding；tracked 的
非 SQLite artifact 仍可能被物化。该模式用于明确采用 volume-only 数据库路径的
集成，并不会让操作可以被归类为 non-materializing，因为显式冲突 resolution
和其他路径仍可能写物理结果。

因此，这个配置是 projection policy，不是 operation gate 的替代品。host 必须
先查询保守 gate。

### 3.3 Commit 边界

`add`/`stage` 捕获物理 SQLite image 并放入 index；`commit` 记录 staged snapshot
并推进历史。即使 `materialize_sqlite = true`，`commit` 也**不得**重写物理
SQLite 文件。这保护应用的 file identity，并允许 checkpoint 后继续使用已有
connection。

应用或 `graft sql` 首次创建物理 SQLite 文件也不是 commit-time materialization；
它发生在 staging 之前。

## 4. 操作生命周期与 gate 语义

### 4.1 三类操作

| 类别 | 含义 | handle 关闭要求 |
| --- | --- | --- |
| Non-materializing | 不得创建、替换或删除 tracked worktree file | 已有应用 SQLite handle 可以保持打开，但仍受普通读写并发规则约束 |
| Potentially materializing | 对给定 state/path 可能改变 tracked worktree file | 调用前必须 quiesce 并关闭受影响的应用 SQLite handle |
| External write | caller 自己写文件，Graft 不拥有写入过程 | quiesce、原子性和恢复由 caller 负责 |

### 4.2 保守 gate

Rust SDK 的 `RepositoryOperation::materializes_worktree` 与 Node 的
`operationMaterializesWorktree(name)` 实现该 gate。返回 `true` 只表示某些
合法输入下该操作**可以**替换、创建或删除 tracked physical file；不表示本次
调用改写了文件，也不是 affected-path 列表。

host 必须在 gate 为 true 时关闭可能受影响的应用数据库 handle，并应使用结果
中的 path actions 缩小范围。如果执行前不能确定受影响路径，host 必须保守地
quiesce 该操作可能触碰的全部应用数据库。

CLI、Node、Rust、WASM 的 gate 必须保持保守一致。不能因为某个常见输入当前没有
path action，就把操作重新分类为 false。

### 4.3 生命周期

物化操作的 host 流程是：

```text
应用继续写入
    |
    v
checkpoint WAL / drain transaction
    |
    v
关闭受影响的应用 handle
    |
    v
调用 Graft 操作并等待结束
    |
    +--> 成功：重开 handle，校验期望的路径状态
    |
    +--> 失败：在重开或重试前 reconcile status/paths
```

Graft repository session 可以继续保持打开；需要关闭的是应用 handle，而不是
Graft session。

## 5. 操作矩阵

下表是当前 1.0 surface 的规范。Gate 列是保守能力；“通常效果”描述一般路径。

| 入口 | 操作 | Gate | 通常物理效果 |
| --- | --- | ---: | --- |
| CLI / SDK | `init` | 否 | 只创建 repository metadata |
| CLI / SDK | `status`、incremental status、metadata、history、inventory | 否 | 读取 repository/worktree state |
| CLI / SDK | `add`、`addAll`、`stagePaths`、`recordPathMove`、`rm --cached`、`untrackPaths` | 否 | 捕获或修改 index，不替换文件 |
| CLI / SDK | `commit` | 否 | 记录 staged canonical snapshot，不重写 SQLite 文件 |
| CLI / SDK | `diff`、`diffPaths`、row diff、history content | 否 | 读取/ hydration snapshot，最多使用临时 `materialized_compat` |
| CLI / SDK | `fetch`、`push`、remote config、payload hydration | 否 | 改 repository/remote storage，不改 worktree 文件 |
| CLI | `checkout <rev>` 或 `checkout <rev> -- <path>` | 是 | checkout target state；path 形式限定范围 |
| CLI | `switch <branch>` / `switch -c <branch> [start]` | 是 | 移动 branch 并应用 checkout plan；相同状态创建 branch 可能没有 path action |
| CLI | `restore <path>` / `restore --all` | 是 | 从 index、`HEAD` 或 revision 恢复 worktree path |
| CLI | `restore --staged ...` | 否 | 只恢复 index entry/worktree classification，不替换文件 |
| CLI | `reset --soft` / `reset --mixed` | 否 | 修改 ref/index classification，不 checkout |
| CLI | `reset --hard` | 是 | reset ref 并应用 checkout plan |
| CLI / SDK | `pull` | 是 | fetch 并 checkout integrated result |
| CLI / SDK | `clone` / `cloneRepository` | 是 | 创建 repository 并 checkout 选定 branch |
| CLI | `merge <rev>` | 是 | 应用 merge outcome 并 checkout；冲突 path 可能保留 ours worktree 内容 |
| SDK / 隐藏 CLI `merge-api` | `planMerge` | 否 | 只计算 up-to-date、fast-forward 或 three-way plan |
| SDK / 隐藏 CLI `merge-api` | `applyMerge` | 是 | up-to-date 通常不写；fast-forward checkout；three-way 写 merge state/index 并按 policy 物化 clean path |
| CLI / SDK | `setMergePathResult` / 整路径 `resolve` | 是 | 写选定 path result，合并冲突 stage |
| CLI / SDK | `resolveMergeRow` | 是 | 写当前 row-resolution candidate；可能物化 merged SQLite result |
| CLI / SDK | `writeAndStageTextResult` / `resolve --manual` | 是 | 写并 staging 编辑后的物理结果 |
| CLI / SDK | `continueMerge` / `merge --continue` | 是 | commit 已解决 merge；当前 CLI path 可能再次写 SQLite snapshot |
| CLI / SDK | `abortMerge` / `merge --abort` | 是 | 恢复 `ORIG_HEAD` 并应用 abort checkout plan |
| CLI | `export` | 否（输出到独立目标） | 只写 caller 选择的 export destination |
| CLI / SDK | `sql`、应用 SQLite transaction | 否（对 Graft 而言） | caller 直接创建或编辑物理 worktree 文件 |

当前 SDK public API 不暴露 CLI `switch`、`checkout`、`reset`；它们仍属于
repository worktree 契约。隐藏的 WASM `merge-api` 创建真实 Rust
`RepositorySession`，执行 SDK 实现，不是浏览器 mock。

## 6. Checkout 与 replacement 算法

### 6.1 Plan 与 preflight

在改变 checkout state 前，adapter 必须解析并 hydration 所需 canonical snapshot，
验证 external artifact payload，保留 untracked path，并计算 affected-path set。
对于 tracked physical SQLite path，在命令的执行顺序允许时，必须先完成
replacement preflight，再改变 repository ref 或 index。

当前 adapter 的 preflight 会检查现有 target 是 regular file，拒绝 path-type
collision，向 SQLite 请求 exclusive transaction，并拒绝 active transaction 或
无法 checkpoint/detach 的 WAL。Restore 还检查 untracked collision；switch/checkout
除非明确 force，否则保留 untracked path。

### 6.2 WAL、锁与 sidecar

对已有物理数据库，adapter 必须：

1. 以 read/write SQLite connection 和有界 busy timeout 打开；
2. 获取并释放 exclusive transaction probe；
3. 若 journal mode 是 WAL，运行 `wal_checkpoint(TRUNCATE)`，要求没有 busy
   reader/writer 且所有 log pages 都 checkpoint 完成；
4. 把旧数据库切回 rollback-journal mode；
5. 删除 regular `-wal`、`-shm`、`-journal` sidecar，非 regular sidecar 必须报错；
6. 在 filesystem replacement 准备好前保留 replacement guard。

活跃 transaction 或正在使用的 WAL 必须使 materializing operation 失败，并且
不能替换主文件。host 应在调用前关闭长连接；但 idle connection 不能替代
quiesce，因为成功 replacement 会改变 directory entry 和 file identity。

### 6.3 逐路径 replacement

adapter 必须在目标目录写出完整独立的 SQLite image 到唯一临时文件，flush 临时
输出，并在 replacement guard 下以原子 rename/replace 替换目标。写入或替换失败
后必须删除临时 checkout 文件。

在 Emscripten/WASM 中，若 OPFS/WASMFS 要求，adapter 必须在 rename 前释放旧的
sync-access handle；browser host 也必须围绕操作关闭并重开自己的数据库 handle。

每个 path 的 replacement 在 filesystem operation 层面是原子的。多路径 checkout
不是跨路径事务；若后续 path 失败，adapter 会尝试恢复已 staged 的物理 backup
和 binding，caller 仍必须通过 `status` reconcile 后再重试。

## 7. Merge 专属物化规则

### 7.1 Plan topology

`planMerge` 是只读的，不得改变 `HEAD`、index、merge state 或物理文件。它返回：

| Plan kind | 条件 | planning 阶段 worktree 效果 |
| --- | --- | --- |
| `up_to_date` | target 是当前 `HEAD` 的 ancestor | 无 |
| `fast_forward` | 当前 `HEAD` 是 target 的 ancestor，或 branch 是 unborn | 无 |
| `three_way` | 两侧已经分叉 | 无；包含 merge base、path analysis、plan token |

计算 plan 时加载远程 target 是 repository storage hydration，不是 worktree
materialization。

### 7.2 Apply

`applyMerge` 之所以被保守地标为可能物化，是因为它能改变 checkout state：

- up-to-date 会重新校验 plan，通常不做物理写入；
- fast-forward 移动 `HEAD` 并应用 target checkout plan；当
  `materialize_sqlite=true` 时，变更的 SQLite path 会投影，artifact 与其他
  checkout-managed file 按自己的 policy 处理；
- three-way 写入 merge index 和 durable merge state。无冲突 path 可以 checkout；
  冲突 path 保留 conflict stages，并可能继续保留当前 ours worktree content，
  直到 resolution 操作写出结果。

plan token 与 expected head 是 compare-and-swap guard。若 `HEAD` 变化、已有 active
merge 或 token 不匹配，必须失败，不能应用候选 plan。

### 7.3 Resolution 操作

整路径 ours/theirs 选择会把选定的 SQLite snapshot 或 file artifact 写到物理 path，
更新 volume binding，并把该 path 折叠为 stage 0。若选定一侧是 deleted，则删除
对应物理 path。按行 ours/theirs resolution 根据 three-way row plan 计算新的
SQLite snapshot，把当前 candidate 写到物理 path，并在文件的所有 row conflict
解决前保留 durable row-resolution state。文本编辑会把提供的 UTF-8 bytes 写到
物理 path 并 staging。

即使某个 path 已经等于选定结果，这些操作仍标记为可能物化。host 必须使用 merge
inspection 返回的 state token；stale token 必须作为“重新读取 status 后重试”处理。

### 7.4 Continue 与 abort

`continueMerge` 要求没有未解决 conflict 且 state token 有效。当前 command path
会 commit 已解决 merge；当 `materialize_sqlite=true` 时，可能把已 commit 的
SQLite snapshot 写回 tracked path。它返回 merge/commit output 和 affected path
actions，但 SDK 结果目前没有独立字段证明某次物理 rewrite 实际发生。

`abortMerge` 要求存在 active durable merge state 且 token 有效。它回到 `ORIG_HEAD`，
清理 merge/index conflict state，并应用 abort checkout plan。即使实际 path set
为空，它仍保守地归类为可能物化。

## 8. CLI、SDK、Node 与 WASM 映射

### 8.1 CLI control plane

普通 CLI 解析用户命令，再通过
`graft_sqlite::repo_service::RepositoryCommandService` 把 repository command
交给 repository-scoped runtime 执行。CLI 没有独立 merge algorithm。隐藏的
`merge-api` 另行打开 `graft_sdk::RepositorySession`，调用 Node 和 Playground
使用的同一组 SDK merge 方法。

CLI JSON 的 materializing command 会在对应 command 支持时返回 `paths` 或
`path_details`。当前 commit 的 `materialized` 数组为空。merge continue 可能
返回 `materialized` 数组；merge apply/abort 返回 path actions，但没有统一的
actual-materialized boolean。

### 8.2 Rust 与 Node SDK

Rust `RepositorySession` 提供长生命周期 session、串行化、expected head/plan/state
token 校验、有界分页、incremental status 和稳定错误码。Node N-API 层把 JS
options 转成 Rust 类型；发布的 JS package 负责 camelCase options、JSON 和错误
转换。以上各层都不重新实现 snapshot merge 或 SQLite replacement。

`operationMaterializesWorktree(name)` 是稳定的 host gate。对以下操作必须返回
true：

`restore`、`restorePaths`、`pull`、`cloneRepository`、`applyMerge`、
`setMergePathResult`、`resolveMergeRow`、`writeAndStageTextResult`、
`continueMerge`、`abortMerge`。

对 read、stage、commit、fetch、plan、merge inspection、history、diff 和 path-move
API 必须返回 false。

### 8.3 Browser/WASM profile

浏览器不能加载 native Node addon。Playground 通过 WASM build 的 CLI 使用隐藏的
`merge-api` bridge，在浏览器 runtime 中创建真实 Rust `RepositorySession`。
OPFS/WASMFS 对 rename 有更严格的 handle 要求：adapter 必须在替换数据库前释放
旧 sync handle，browser host 也必须围绕操作关闭/重开自己的数据库 handle。
UI 不能宣称已经验证了 Node-native connection lifecycle。

Browser profile 必须披露这个边界，使用同一个 JSON operation contract，并在浏览器
repository 中保留 durable merge state。只改变 React state 的 UI mock 不是
conformance 测试。

## 9. 结果、affected paths 与错误

操作结果可以包含：

```text
paths / path_details：repository-relative path actions
materialized：旧 CLI 路径明确报告的 SQLite/file actions
materializes_worktree：SDK batch result 上的保守 capability bit
merge：merge operation 之后的 durable merge status
output：底层 typed command JSON
```

Path action 可以包含 path、kind/storage 和 `checked_out`、`staged`、`conflicted`、
`removed`、`materialized` 等 action。它描述 command 的 state transition，不证明
host filesystem 已经 flush 每一个 byte。当前 SDK 还没有为所有 materializing
operation 提供统一的 `affected_paths` 加 `actual_materialized`；需要精确证明时，
caller 必须使用各操作的 documented output，并用 status reconcile。

错误不得被静默重试。重要错误类别包括：

- 参数非法或 path/type 不支持；
- active SQLite transaction、live writer、WAL、sidecar 或 path collision；
- 另一个 session/external writer 导致 repository busy；
- `HEAD` 变化、stale plan token 或 stale durable merge state token；
- snapshot/payload 缺失或尚未 hydration；
- 逐路径 replacement 中的 filesystem failure；
- 穿过 commit 或 filesystem boundary 后无法确定结果。

发生未知或部分结果后，host 必须 reopen/reconcile session，查询 status 与 merge
status，检查 affected path，然后才能提供 retry、continue 或 abort。不能盲目重复
checkout。

## 10. 并发、持久性与恢复

一个保留的 SDK session 会串行化一个 repository 的操作。第二个 session 或外部
Graft writer 必须在第一个 owner 关闭或释放 storage lock 前收到 repository-busy
错误。外部普通 SQLite 写入由后续 status/stage 观察；不会被静默合并进已经捕获
的 staged snapshot。

Merge state 以 refs/index metadata 持久保存（包括 `ORIG_HEAD`、`MERGE_HEAD` 和
index stages）。关闭并重开 session 后，merge status 必须从这些状态重建。host
可以在关闭/重开应用 SQLite handle 时保持 repository session 常驻，但 crash recovery
必须先同时重建 repository state 和 physical path state，再恢复 UI 操作。

Replacement 使用同目录 temporary file 和逐路径 backup restoration；当前实现不
承诺 filesystem-wide atomic multi-path checkout，也不承诺所有 host filesystem 都
有 durable fsync barrier。因此 conformance host 必须把 status reconciliation
作为 recovery 的一部分。

## 11. Conformance 要求与当前证据

`GWM-Writer-1.0` 至少需要以下测试族：

1. **分类：** 覆盖完整 gate 表，特别是 `commit`、staging、fetch、plan、diff、
   merge inspection 为 false，restore/pull/clone 和 merge resolution 为 true；
2. **Commit identity：** 打开物理 SQLite handle，stage 和 commit，证明主文件
   identity 与 open handle 仍有效，并证明 commit 没有报告 `materialized`；
3. **Capture：** 不手动 checkpoint 也能 stage WAL 数据库，证明 canonical snapshot
   包含已提交 frames，且 source WAL 没有被静默销毁；
4. **Replacement：** checkout/restore/switch 改变数据库，验证内容、volume binding、
   stale sidecar 清理和 affected path actions；
5. **Locking：** 持有 live transaction 或 WAL writer 时 replacement 被拒绝且
   主文件不变；replacement guard 持有期间新 writer 被阻塞；
6. **Merge：** 覆盖 up-to-date、fast-forward、clean three-way、conflicted three-way、
   整路径、按行、文本、continue、abort、stale plan 和 stale state token；
7. **Recovery：** 注入 replacement/write failure，重开 session，reconcile status，
   不接受静默混合的多路径状态；
8. **Profiles：** 通过 Node 与 WASM/OPFS 运行相同 merge-api contract，并明确记录
   native Node addon 边界。

当前仓库证据包括：

- `crates/graft-sdk/src/lib.rs` 的 materialization classification tests；
- `packages/graft-sdk/test/repository-session.test.js` 的 ABI/gate、open-handle、
  restore、staging 和 identity tests；
- `crates/graft-tool/src/main.rs` 的 CLI commit、switch、clone 和 physical-file tests；
- `crates/graft-sqlite/src/pragma/sqlite_worktree.rs` 的 WAL、replacement guard、
  sidecar 和 physical snapshot tests；
- Rust SDK 中关于 merge topology、durable status、resolution、stale token、continue、
  abort 和 reopen 的测试；以及
- `web-demo/tests/e2e/playground-ui.spec.ts` 的真实 WASM merge、resolution、abort
  和 browser UI 流程。

## 12. 兼容性说明与当前规格漂移

当前源码、测试、configuration/snapshot guide、SDK architecture guide 与 Playground
文案都确定 `commit` 是 non-materializing。本 1.0 baseline 之前的旧副本不能覆盖
本规格。目前仍有一个 result shape 缺口：

- CLI JSON 与 SDK merge result shape 还没有把 actual affected paths 和 actual
  materialization 统一到一个字段。

兼容性修正结论是：应用或 `graft sql` 直接创建第一个物理文件；`add` 捕获它；
`commit` 记录 snapshot 但不 replacement；checkout 类操作和显式 merge resolution
才是 replacement 边界。

## 13. 原理（信息性）

让 `commit` 不物化可以保护应用 SQLite inode identity，并允许长生命周期应用
handle 在 version checkpoint 之后继续有效。保守 gate 仍然必要，因为 checkout
和 merge resolution 可以替换同一路径。把 hydration 与 `materialized_compat` 分开，
避免调用者为了只读 inspection 而关闭应用 handle。CLI 与 SDK 共享同一 core，也
使浏览器可以在不加载 native Node code 的情况下执行真实 merge workflow。

## 规范性参考

- [Graft SDK TypeScript contract](../../packages/graft-sdk/index.d.ts)
- [Graft SDK materialization table](../../packages/graft-sdk/README.md)
- [Repository merge core](../../crates/graft/src/repo/merge.rs)
- [SQLite checkout and replacement adapter](../../crates/graft-sqlite/src/pragma/repo_checkout.rs)
- [SQLite worktree locking and WAL adapter](../../crates/graft-sqlite/src/pragma/sqlite_worktree.rs)
- [Eidos Specifications index](https://github.com/eidos-space/eidos/tree/main/docs/specs)
