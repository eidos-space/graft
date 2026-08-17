# Graft Merge 1.0（中文参考）

状态：与当前实现对齐的规范草案
版本：1.0
发布日期：2026-08-11
规范语言：英文

## 摘要

本规格定义 merge plan/apply、fast-forward 与 three-way topology、path/SQLite row
conflict、durable merge state、version inspection、resolution、stale token、continue、
abort，以及 process/session 重开后的恢复。

核心安全规则：plan 只读，apply 不能静默覆盖分叉双方。每个 unresolved path 都要
保留可用 base/ours/theirs 与 durable recovery 路径。

## 1. 范围与术语

`ours` 是 plan 时 local `HEAD`；`theirs` 是 target commit；`base` 是 merge base；
`result` 是当前 staged/candidate path。Object/index、diff、物理文件 replacement
分别由 Repository、Diff、Worktree Materialization 规格负责。

## 2. 状态机

```text
idle -> plan: up_to_date | fast_forward | three_way
                 |              |              |
              unchanged    fast-forward   durable merging
                                               |
                                   resolve -> continue/abort
```

同一 repository 只能有一个 active merge。已有 merge 时 apply 新 candidate 必须失败；
plan 不能替换 active merge。

## 3. Planning

`planMerge` 解析 target/graph 并计算 path/SQLite comparison。Retained SDK 与 browser
API 返回 plan token；one-shot human CLI 可以在一个 command 内 plan+apply 而不暴露
token。可以 hydrate immutable object/page，但不能移动 ref、改 index、写 merge
record、resolve 或 materialize。CLI apply 仍会在 core 中校验 expected `HEAD`。

| outcome | 条件 | 计划效果 |
| --- | --- | --- |
| `up_to_date` | target 是 ours ancestor（含相等） | 无变化 |
| `fast_forward` | ours 是 target ancestor | HEAD/branch 移到 target，并按规则投影 |
| `three_way` | 双方从 base 分叉 | stage clean result，durable 保存 conflict |

Unborn branch 在允许时可 FF。若没有共同祖先，当前实现返回
`merge_base = null`，并以 empty base 做 three-way path comparison；双方同 path add
会成为 add/add conflict（除非完全相同）。Client 必须明确显示 absent base，不能伪造
共同祖先。

若存在多个不可比较的 best merge base，实现按“到两 head 的较大距离、距离和、
object ID”依次取最小值，确定性选择一个；1.0 不合成 recursive merge base。

当前 plan token 是精确 serialized immutable merge plan、effective policy token/version
的 BLAKE3，覆盖 revision/target、topology、checkout action、candidate index 与 outcome。它对 client opaque，
不是 repository credential 或 global nonce。Apply 在当前状态重新计算 plan 并比较。
Malformed/mismatched/stale token 必须无副作用失败；unchanged up-to-date no-op 可能仍
可重复使用，而 state-changing outcome 会因状态变化或 active record 自动失效。
Client 遇 stale 后重新 status/plan。

## 4. Path-level three-way

| 关系 | 结果 |
| --- | --- |
| `O == T` | 共同版本 |
| `O == B`, `T != B` | theirs |
| `T == B`, `O != B` | ours |
| 仅一侧 add | added side |
| 双方 add identical | shared addition |
| 一侧 delete、另一侧 unchanged | deletion |
| divergent modify/modify、add/add、modify/delete | conflict，除非 type-specific merge 成功 |

Clean path 变成 Normal index/staged deletion。Conflict 保留可用 Base/Ours/Theirs
stage；缺 stage 表示该侧 deletion/不存在。不能按时间戳自动选一侧。Path kind 或
storage class 分叉也属于 conflict，除非有明确规则。

## 5. SQLite three-way merge

只有 base/ours/theirs 都是 compatible SQLite snapshot，且 logical diff 能安全识别
相关 surface 时才做 row merge。Missing、add/delete、malformed、unsupported 或
incompatible 情况 fallback whole-path conflict。

独立 row change 合并；identical touch 合并为一次；同 identity 的不兼容 update/
delete 等形成 row conflict。两个独立 insert 若仅 rowid 碰撞，只有在双方都是
insert、table 没有 `INTEGER PRIMARY KEY` rowid alias、两侧 semantic key 都可得且
互不相同时，才可省略 theirs requested rowid 让 SQLite 分配新值；不能 remap
declared PK 或隐藏 referential/opaque 不确定性。

Semantic key 的选择顺序是 `merge.semantic_keys.<table>`、可解析的
`merge.default_semantic_keys`，再到 parsed schema 中第一个不是 rowid alias 的 declared
primary-key/unique constraint。它用于发现不同 storage identity 但业务键重复的
insert。冲突不能 auto-remap；当前必须 whole-file/manual resolve。

Schema 只通过支持的 deterministic resolver 合并。当前 built-in 只支持 compatible
`add_column -> alter_table_add_column`；divergent definition、delete/modify 与其他
operation 保持 conflict。

默认 opaque/internal resolver：

| subject | resolver |
| --- | --- |
| `index_btree` | `reindex` |
| `sqlite_sequence` | `sequence_max` |
| `sqlite_stat1` ... `sqlite_stat4` | `rebuild` |

Unknown/disabled/incompatible opaque change 必须保持 conflict；resolver 不是任意 SQL
hook。

Candidate 在隔离 temp DB 构造。变更前先为精确、不可变的 Ours
`(VolumeId, RepoSnapshot)` 执行完整 SQLite `integrity_check` 并建立证明；进程只能为
相同 content-addressed state 复用该证明，证明不能来自 repository-controlled file，
cache miss/eviction 必须重跑完整检查。

之后通过单个 SQLite transaction 做受控 replay：关闭 FK/trigger side effect，开启
`cell_size_check`，保留 SQLite 原生 constraint/index maintenance，并排除
generated/configured column；完成后执行完整 `foreign_key_check`。由于 source 已有完整
不可变 integrity proof，且后续只有 SQLite transaction 修改它，证明后的验证除必要的
跨行 FK 检查外可与 delta 成正比。验证失败不能替换 staged result/worktree 或改变之前的
merge journal，temp 成功失败都要清理。

若 preflight 已稳定一个 clean worktree file，实现只有在 private copy 与 Ours 的精确
content-addressed page index 匹配后，才能用 filesystem clone 或 kernel copy 作为
candidate seed。Page index 缺失、copy 不匹配、clone 失败或平台不支持时，必须回退到权威
Graft snapshot 物化。Filesystem clone 本身永远不能作为 integrity 或 identity proof，
所以首次使用的 candidate 仍须执行上文的完整检查。

Candidate copy、精确 identity 验证与完整检查可以和彼此独立的 immutable row-diff planning
重叠。若 planning 不需要该 candidate，必须取消 validation、删除 private file；除非完整
检查已经成功，否则不能记录 proof。实现不能仅因 disposable 精确 page index 缺失，就在
后台推测性启动 full-snapshot materialization。

若 SQLite WAL mode 可用，实现可以在应用 transactional delta 时保留截至最后一个已提交
frame 的全部 page number。只有 SQLite 成功 checkpoint 同一个 WAL，且结果 database
通过必要验证后，才可用该集合做 sparse repository import。Page number 只是保守候选：
每个候选页仍必须与不可变 Ours snapshot 比较，page-count 变化必须显式处理。WAL 缺失、
格式错误、不完整、未提交或不一致时，必须回退到权威 full candidate import。WAL frame
只能定位 output page，不能跨 branch 合并，也不能定义 row conflict 语义。

Directory repository session 必须规划每个冲突 SQLite path；任何 unmerged path 都必须
返回结构化 conflict、validation/analysis error、limitation 或明确可执行 action。

同一行字段合并默认关闭。Policy version 1 显式启用 `same_row_merge` 后，update/update
依据 Base 合并：变更列不重叠则组合，同列值相同则折叠，同列值不同则产生结构化 cell
conflict；delete/update 仍是 conflict。`resolveMergeCell` 可安全选择字段 side，并继续使用
state-token CAS、持久 journal、reopen/unresolve/abort 语义。

Semantic key 会检测 insert/insert、insert/update、update/update 的 result collision。
默认 `BINARY`，可显式配置 SQLite 内建 ASCII `NOCASE`。Conflict 返回双方物理 identity、
展示用 semantic key 和 collation，不解释业务含义。

Managed-column resolver 是有限集合：`ignore_for_conflict`、`max`、`min`、
`max_timestamp`、`recompute`，不执行任意应用代码。`recompute` 省略该列、物化其余候选，
并把 path 保持为待重算。应用完成重算后用 `stageMergeSqliteResult` 捕获精确 worktree
snapshot，在 private temp DB 重跑 integrity/FK check 后才 stage。旧 `generated_columns`
会归一化为 `recompute`。

Typed SDK 提供 `getMergePolicy`、`validateMergePolicy`、`setMergePolicy`。每个 effective
policy 都有稳定 `policy_token`；set 需要前一个 token。`planToken` 绑定 plan、policy token
和 version。Three-way apply 把 policy/token/version 冻结进 journal，active merge 中必须先
完成或 abort 才能改 policy 并 replan。

## 6. Durable merge state

Three-way apply 持久化：

- `ORIG_HEAD` 中的 original local commit；
- `MERGE_HEAD` 中的 target；
- Base/Ours/Theirs/Normal index stage；
- `merge-resolution-session.json` 中的原始 conflict stage、whole-path
  resolution、冻结 policy/token/version、row/cell selection 与逐 path analysis state；
- 可确定性重建 merge state token 的 status/index/resolution 输入。

当前 state token 前缀为 `graft-merge-v1:`，hash 完整 repository status、index 与可选
`merge-resolution-session.json`。状态不变时跨 close/reopen 稳定；index/status/path
resolution/row selection/unresolve 变化后 token 改变。Journal 必须保留到成功
`continueMerge` 或 `abortMerge`。

Worktree 不是唯一记录。关闭进程和 SDK session 后重开必须恢复相同 unresolved count
与 versions。Record 缺失/不一致必须作为可恢复 corruption 报告，不能把当前物理
bytes 猜成 intended result。

Application semantic provider 使用单独的 `.graft/semantic-merge` workspace。它不进入
merge-state-token hash，也不能自行 resolve path。每个 manifest 绑定精确 active state
token、冻结 policy token/version、Base/Ours/Theirs revision、provider 标识与 repository
path。Workspace 和已记录 domain conflict 在 SDK close/reopen 后继续存在，直到 merge
成功 continue 或 abort。

## 7. Status 与 conflict inspection

`getMergeStatus` 至少报告 active、ours/theirs/base、current token/state identity、
unresolved count 与 can-continue。List API 按 path 确定排序并 bounded；当前 SDK
merge path 最大 500、conflict 最大 1000。Result 必须说明 path、category、可用
version 与 row/schema/opaque detail；截断要显式。

Active merge 中 path 即使已收敛为 Normal/staged result，inspection 仍须返回原始
conflict 记录以及当前 `status`/`resolution`，使客户端 reopen 后无需猜测 worktree
bytes 即可重新展示 resolved SQLite table。

`readMergeVersion` 精确读取 `base/ours/theirs/result`；absent side 不能替代。只读。

`diffMergeSqlite(path, from, to, response)` 在 active merge 内比较两个不同的 immutable
`base`、`ours` 或 `theirs` revision。即使 index 仍有 unresolved conflict 也必须可用；
必须校验 merge state token、支持 cancellation 与 bounded response，且不得写 worktree、
index、ref 或 merge journal。通用结果包含 table/row fact、schema entry（`name`、
`entry_type`、`op`、新 SQL 与可用时的旧 SQL）、opaque change、analysis limitation 和明确
logical status。Client 通过 Base→Ours 与 Base→Theirs 两次比较组成三方视图。

若一侧与 Base 的物理字节不同，但受支持的 logical diff 为空，且无 schema/row conflict、
opaque change 或 limitation，engine 可在不执行 SQL 的情况下把 file-level conflict 安全
收敛到另一侧。Theirs 与 Base 逻辑等价时安全结果为 Ours，从而保留所有受支持的本地
non-conflicting change，而不是盲选整个文件。对自动处理逻辑上线前已存在的 active merge，
inspection 必须返回 `auto_resolvable = true` 与 `recommended_result = ours`；任何 opaque
change 或 limitation 都禁止使用此等价规则。

## 8. Resolution

所有 resolution mutation 必须基于 current state。Retained SDK/browser `merge-api` 必须
带 current token，stale token 不能改变 path/selection。One-shot human CLI 不暴露
client-held token；它获取 repository coordination、重新读取当前 state 后立即执行。

- `setMergePathResult` 选择整 path ours/theirs，支持 file、SQLite 与 deletion，收敛
  为 Normal 或 staged deletion；merge 完成/abort 前仍须可恢复未选 side。
- `writeAndStageTextResult` 对 eligible UTF-8 text 写入并 stage 精确 result；binary、
  invalid input、token/path mismatch 或写失败要保留原 conflict。
- `resolveMergeRow` 对 eligible row conflict 选 ours/theirs，并持久化 selection、重建/
  更新 candidate、验证。全部 row resolved 且无 schema/opaque conflict 后 path 收敛。
- `resolveMergeTable` 对一个 SQLite table 的所有可逐行解决 conflict 原子选择同一 side，
  保留双方 non-conflicting change，只构造并 materialize 一个 candidate、只发布一个新
  state；schema/opaque/semantic-key 必须在任何 mutation 前拒绝。之后可整体切换另一 side。
- `unresolveMergePath` 从 active session journal 恢复原始 Base/Ours/Theirs stage 与 merge
  worktree candidate，清该 path 的 row/whole-path selection；只允许同一 active merge，
  stale token 不得产生副作用。

Semantic-key、schema、opaque、malformed 或 unsupported conflict 不能 per-row resolve，
必须明确要求 whole-path/manual result。

Resolution 是否写普通 SQLite worktree，以及 WAL/lock/sidecar/replacement/rollback，
只由 Materialization 规格定义。

### 8.7 Application semantic provider

`prepareSemanticMerge` 是 unresolved SQLite path 的 non-materializing、token-guarded
handoff。Application 必须声明一组非空且有界的 provider-managed table name。Graft 在
Graft-owned private workspace 中导出存在的 Base/Ours/Theirs standalone read-only
physical file；同时从 Ours 创建固定 `result.sqlite` candidate，并在其中应用 declared
managed table 之外无冲突的 Theirs row change。返回的 provider token 绑定 provider
name、path、active merge-state token、冻结 policy token/version、三个 immutable
revision 与 canonical managed-table set。Graft 不调用 application code，也不解释
managed table 的业务含义。

若 physical plan 含 schema addition/conflict、opaque/limited change、需要 recomputation
的 change，或 managed table 之外仍有 unresolved row conflict，seed construction 必须在
发布 workspace 前失败。失败不能改 index、worktree、conflict stage 或 merge-state
token。`seed_applied_sql` 表示 safe unmanaged Theirs projection 是否修改 candidate；
`managed_conflicts` 表示 provider-managed set 内的 row conflict 数量。这些值只用于诊断，
不能替代 application validation。

相同且未变化的 handoff 再次 prepare 时返回同一 workspace 与 provider record。
`recordSemanticMergeConflicts` 持久记录 bounded application domain conflict 与自动决策
audit，但不改 index、worktree、conflict stage 或 merge-state token。Graft 把记录视为
opaque JSON，不能提升成 built-in row/schema rule。

Application 更新并验证 seeded `result.sqlite` 后，`acceptSemanticMergeResult` 同时校验 provider
token 与 current merge-state token。它先捕获精确 result 并执行 SQLite integrity/FK
validation，再通过正常 materialization boundary 替换 application worktree，并 stage 一条
Normal path result。Missing、非 SQLite、stale 或 invalid result 必须保留可恢复的原始
conflict stage。Application validation proof 与自动决策 audit 是 bounded opaque record；
业务含义仍由 provider 拥有。

此接口是 generic 的。Provider 可以实现 Eidos metadata policy、其他应用 schema policy，
也可以完全不配置。Graft 不能包含 provider-specific table name、clock、LWW rule 或
dependency logic。

## 9. Continue 与 abort

`continueMerge` 必须验证 active，且 token-based adapter 的 token current；同时无
conflict stage、所有 row/schema/
opaque resolution 完成、candidate/payload 可用有效、`HEAD` 仍是 ours。它用精确
staged result 创建两个 parent（ours first、theirs second）的 commit。Commit/ref
成功后才能清 merge record 与 resolution journal。失败后必须是 recoverable active merge 或完整 commit，
不能 half-state。

`abortMerge` 恢复记录的 original state，清 conflict/index row state，并在恢复成功
后删 merge record。物理恢复遵守 Materialization。恢复失败要保留 record，明确
允许 retry/manual recovery。

## 10. FF 与 up-to-date

`up_to_date` 不产生 commit/index/ref/worktree/merge-state 变化。`fast_forward` 直接
移动 branch/HEAD 到 target，按 checkout 规则更新 index/worktree，不创建 merge
commit 或 active state。只有 `three_way` 能进入 active merge 并最后形成双 parent。

## 11. 并发、错误与恢复

Merge mutation 由 repository coordination 与 expected-state 串行。HEAD/index/token/
row/path 并发变化必须报 stale/busy。Safe boundary 前取消无效果；durable apply 后
取消必须返回可观察 status，不能删除 continue/abort 所需 record。

Crash/reopen 后恢复入口始终是：

```text
inspect -> continue resolving -> continue
inspect -> abort
```

Startup 不能为了清 conflict 自动选 ours/theirs。

## 12. Conformance

至少测试：三种 topology、plan 只读与 stale token、所有 path 关系、SQLite 独立/
冲突 row、composite key/rowid remap/semantic key、schema/opaque resolver、精确不可变
base integrity proof/reuse、transaction constraint、post-apply FK validation 与 rollback、
whole-path/text/row resolution、关闭重开、continue 双 parent、
semantic-provider prepare/reopen、stale provider/state token、bounded conflict record、invalid
result rejection、validated result acceptance、continue/abort cleanup、abort 恢复，以及
durable/physical boundary 的 crash/failure。

当前证据位于 `crates/graft/src/repo/merge.rs`、repository merge test、
`crates/graft-sqlite/src/` row-merge test、command service、Rust/Node SDK test 和
Playground browser fixture。

## 13. 已知限制

- Unrelated history 使用 empty base（`merge_base = null`），不合成 ancestor。
- Semantic-key conflict 需要 whole-file/manual resolution。
- 有 schema/opaque conflict 时不能 per-row selection。
- 只接受文档化 schema/internal resolver。
- 多 path 物理投影可恢复但不是单一 filesystem transaction。
