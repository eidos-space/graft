# Graft Diff 1.0（中文参考）

状态：与当前实现对齐的规范草案
版本：1.0
发布日期：2026-08-11
规范语言：英文

## 摘要

本规格定义 repository path、普通文件内容、SQLite row/schema、opaque SQLite
surface、历史变化，以及 bounded/paginated result API 的只读比较；同时规定逻辑
SQLite diff 无法直接完成时必须披露的 capability 和 limitation。

Diff 只观察，不 stage resolution、不移动 ref、不改 durable merge state，也不写
application worktree file。

## 1. 范围与输入

Diff 比较两个 side。Side 可以是 commit/revision tree、index、当前 worktree、merge
version（`base/ours/theirs/result`）或显式 snapshot/artifact。调用者必须无歧义地
指定；省略值按具体操作的文档默认。

当前 comparison mode：default 是 `HEAD` 与 worktree（general result 同时分类
staged/unstaged）；`staged` 是 `HEAD` 与 Normal index；`root=R` 是 empty tree 与 R；
只有 `from=A` 是 A 与 worktree；`from=A,to=B` 是两个 revision；merge `version`
精确选择 active side。`root` 与 `from/to` 互斥，`to` 必须有 `from`。SQLite
`stagedFallback` 只在请求 path 没有 worktree change 时使用 staged comparison，并
必须说明实际 source。Path/table filter 只缩小工作量，不改变 side identity。

Path/artifact identity 由 Repository 规格定义，snapshot/hydration 由 Storage
规格定义，merge version 由 Merge 规格定义。

## 2. 只读不变量

Diff、history summary、commit detail、changed path、content read 与 row inspection
可以 hydrate immutable object/page 或创建临时 compatibility file，但不能：

- 移动 `HEAD`/ref；
- 改 index 或 durable merge state；
- 创建、替换、删除 tracked worktree path；
- 解决 conflict；
- 发布 remote mutable state。

Hydration 与 `materialized_compat` 都不是物理 worktree materialization。

## 3. Path-level diff

每个 canonical path 分类为：

```text
added | deleted | modified | unchanged
```

若 API 不返回 unchanged 必须声明。结果应带 path kind/storage 和 side identity。
Add/delete 只有一侧有内容；kind/storage representation 变化至少是 modified。

History 可以把一 delete、一 add 配成 exact move：SQLite 用 exact snapshot identity，
普通 file 用 exact content identity。1.0 没有 similarity score、copy detection 或
many-to-many rename heuristic。

默认按 canonical path 确定排序。Stable comparison 的分页不能重复或漏 path。
当前 SDK path page 最大 100，单次请求预算最大 10,000 path。

## 4. 普通文件内容

Inline 或已 hydrate external bytes 若为合法 UTF-8，可作为 text。Unified/side-by-side
hunk 与 folding 属于 adapter/UI；canonical observation 是精确 bytes 或 decoded text。
不能靠 newline normalization、字符替换或 encoding conversion 宣称 byte equality。

Binary path 报告 identity、size、availability 与 changed status。当前 SDK content
read 上限 8 MiB。Missing external payload 不等于 empty file，应返回可用于 fetch/
repair 的 identity。Path/side/size bound/hash 必须校验；截断必须带 metadata 或
size-limit state/error。

SDK `readPathContent` 必须给 explicit immutable revision。Active merge 用
`readMergeVersion`；当前 worktree bytes 由 host/filesystem 读取，不能冒充 immutable
revision。

## 5. SQLite logical diff

结果有四个独立 domain：row、schema、opaque change，以及 capability/limitation/
response scope。Row 为空不代表 SQLite file unchanged。

Row identity：普通 table 使用 signed 64-bit `rowid`；`WITHOUT ROWID` 使用声明顺序
的 typed composite primary key。Key part 类型：

```text
null | integer | real | text | blob
```

Integer/text/blob 精确比较；real 使用规范化 bits，`+0` 与 `-0` 是同一 key；
composite 顺序必须保持 schema 顺序。

Row change 是 insert/delete/update，并提供 identity 与足以检查的 before/after value。
Column 顺序按 schema。Generated column 可以显示，但需标记供 merge/apply 排除。
SQL `NULL` 用 JSON null 与 field presence 区分。当前普通 row `values/old_values` 中
BLOB 使用 lowercase hex **bare string**，与 SQLite TEXT 有歧义；primary-key BLOB
则使用 row resolution 也接受的 `{ "$blob": "<hex>" }`。Client 必须结合 schema/type，
不能把普通 row JSON 当作 lossless typed round trip；改变该 legacy shape 需要版本化。

Schema entry 是 added/deleted/modified，含 type/name 与可用 before/after SQL。Table
definition 变化不能降格成纯 row output。

Opaque entry 是 added/deleted/modified，含 reason 和可知时的 owner。Virtual table、
FTS shadow、SQLite internal table、未表达成 row 的 index B-tree 变化都必须暴露，
不能静默忽略。

## 6. Capability、limitation 与 logical status

当前 capability：

```text
rowid_table_rows
primary_key_table_rows
schema_entries
opaque_table_detection
semantic_insert_keys
```

当前 limitation：

```text
virtual_table
fts_shadow_table
sqlite_internal_table
index_btree
utf16_text_encoding
generated_columns
```

顶层 status：

| status | 含义 |
| --- | --- |
| `logical_changes` | 至少有一个支持的 row/schema/opaque observation |
| `unsupported_logical_surface` | changed surface 无法完整解释 |
| `file_changed_no_supported_logical_changes` | bytes/pages 变化，但无支持的逻辑结果 |

Repository API 在 add/delete 或 snapshot 缺失时还可返回 `row_diff_unavailable`。
Limitation 是 completeness 证据，不能为了空结果而隐藏。

## 7. Execution scope

| scope | 含义 |
| --- | --- |
| `streaming_rowid` | 直接遍历 4 KiB B-tree/page 的 rowid row |
| `streaming_primary_key` | 按 declared primary key 直接遍历 |
| `streaming_btree` | 直接 schema/opaque B-tree 遍历 |
| `materialized_compat` | 临时只读 SQLite compatibility DB |
| `unavailable` | 无法提供 logical execution |

Direct streaming 应避免加载全库。`WITHOUT ROWID` 在 layout 支持时使用 typed PK，
否则 fallback `materialized_compat`。Compatibility DB 必须在隔离 temp path 创建、
只读打开，并设置 `trusted_schema=OFF` 等 defensive option；成功失败都要清理，不得
改变 repository/worktree。

## 8. Bounded row API

请求可以取跨 table summary 或某 table 的 rows。当前每页 limit 为 1–1000。
Offset/cursor 必须校验，并在 stable snapshot pair 上确定。Response 要能继续分页、
说明 has-more、table、identity mode、scope 与 limitation。Cancellation checkpoint
应在长循环内部。

Summary 可以比完整 materialization 便宜，但达到 bound 时不能把 lower-bound 冒充
exact count；exact/bounded/lower-bound 必须可区分。

## 9. History 与 merge version

History summary 可只读 commit metadata；请求 row details、path content 或历史
schema 时才 hydrate 所需 object/payload/frame。

Merge version：

```text
base    common ancestor
ours    local pre-merge
theirs  merge target
result  当前 staged/candidate result
```

读取都只读；absent version 表示 deletion 或无 candidate，不能从其他 side 伪造。
更新 `result` 属于 Merge 规格。

## 10. 失败与 stale state

Malformed SQLite page/schema、missing object/payload、hash mismatch、unsupported
surface/encoding、invalid cursor 与 cancellation 必须以可区分 error/limitation 报告。

Retained SDK 可在 multi-step read 前后 fingerprint repository；若允许重试后仍因
并发无法得到 coherent observation，必须返回 stale error，不能拼接不同版本。
当前 SDK 对相关 path race 最多尝试三次。

## 11. Conformance

至少测试：确定排序与分页、exact path/move classification、UTF-8/binary/missing
payload/size limit、rowid CRUD、composite PK 与 WITHOUT ROWID、schema change、
opaque virtual/FTS/internal/index、logical status/limitation、streaming 与 compat
结果等价、history/merge-version 只读，以及 cancel/stale 无部分写入。

当前证据位于 `crates/graft/src/repo/diff.rs`、repository history test、
`crates/graft-sqlite/src/row_level_diff.rs`、bounded row-diff test 和 SDK contract test。

## 12. 已知限制

- Direct logical parser 针对 4096-byte SQLite snapshot。
- UTF-16 和若干 virtual/internal surface 作为 limitation/opaque 暴露。
- `materialized_compat` 可能较慢并使用临时磁盘。
- 非 key BLOB row value 是 legacy bare hex string，无 schema 时与 TEXT 有歧义；
  primary-key BLOB 有 tag。
- 1.0 只有 exact move detection。
- Hunk 算法、folding 与 UI presentation 不属于 canonical API。
