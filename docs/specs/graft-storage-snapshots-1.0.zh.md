# Graft Storage and Snapshots 1.0（中文参考）

状态：与当前实现对齐的规范草案
版本：1.0
发布日期：2026-08-11
规范语言：英文

## 摘要

本规格定义 Graft immutable 4 KiB page、log 与 LSN、volume、storage commit、
snapshot、reader/writer、hydration、snapshot publication 与 storage GC。它位于
repository object 之下：repository SQLite blob 命名并校验 storage snapshot，ref
与 repository commit 仍由 Repository 规格负责。

## 1. 范围

本规格负责 page、log、LSN、volume、storage commit、snapshot range、hydration
与 storage reachability。它不定义 repository object/path、SQLite row/schema
解释、merge policy、remote branch ref publication 或普通 worktree 文件写入。

## 2. 基础类型与不变量

Graft storage page 固定为 4096 bytes。`PageIdx` 是非零、一起点位置，第一页为
`1`；`PageCount` 是 snapshot/volume 的逻辑页数。单页读写必须正好 4096 bytes。
Byte offset 是零起点，转成一起点 PageIdx 时必须校验范围和溢出。SQLite page
number 与 Graft PageIdx 直接对应。非 4096-byte SQLite 可以走上层 compatibility
path，但不能直接视为 page-compatible。

`LogId`、`VolumeId`、segment ID 是 opaque stable ID；host 不能从文本形式推导
权限、顺序或 repository path。

LSN 是某一个 `LogId` 内非零、单调递增的逻辑序号。它不是 global ID 或 content
hash；只有 `(LogId, LSN)` 唯一标识 storage commit。不同 log 的 LSN 本身不可比较。

## 3. Storage commit 与 segment

Storage commit 至少记录 log/LSN、结果 page count、changed-page set/segment index、
frame range，以及可用时的 hash、checkpoint、timestamp、message。它表示同一 log
前态叠加变更后的完整逻辑状态，不是 repository commit。

Segment 保存 immutable page frame；index 把 changed `PageIdx` 映射到 frame。
读取必须验证 commit/segment metadata；应存在的 page 缺失或损坏时必须报错，
不能伪造 zero page。Dedup、pack、cache 或远程 fetch 不能改变 snapshot 语义。

## 4. Snapshot

结构：

```text
page_count
ordered ranges: [ (log_id, start_lsn, end_lsn), ... ]
```

Range 两端 inclusive，必须非零且 `start <= end`；顺序按优先级从高到低，第一
range 覆盖后续 fallback range。Snapshot head 是第一 range 的 end。Empty snapshot
的 page count 为 0、无 range。

Repository `sqlite-snapshot-v1` 还为每个 LSN 记录 expected storage commit hash；
consumer 必须校验后才能作为 canonical repository content 使用。

Reader 从最新适用 commit 向旧状态查找 page。`PageIdx >= page_count` 在逻辑文件
之外。缺本地 commit/frame 可触发 lazy fetch，fetch 失败必须暴露。打开 reader
只读，不能推进 volume、追加 commit、移动 ref 或物化 worktree。

Snapshot equality 由 canonical descriptor/identity 定义，不取决于本地 cache。
Checksum API 必须覆盖其声明的 bytes/range。

## 5. Volume 与 log 关系

Volume 是 current snapshot lineage 的 mutable handle，记录 `VolumeId`、local log、
remote log、sync point 和可选 pending publication。Volume status 按 sync point
比较 storage lineage，报告 up-to-date/ahead/behind/diverged；这不同于 repository
branch ahead/behind。

创建 volume 建立新的 writable lineage；打开已有 volume 必须保持状态。把历史
log ref checkout 成 volume 必须创建新 volume/lineage，不能修改源历史。

## 6. Reader、writer 与 page count

`VolumeReader` 观察 stable snapshot；后续 commit 不能改变它读到的 bytes。Lazy
fetch 必须仍保持同一逻辑 snapshot。

`VolumeWriter` 从 base snapshot 开始，overlay dirty page 并维护 page count。Reader
through writer 先看 dirty page，再看 base。Commit 追加 immutable storage commit，
返回结果 snapshot reader。Writer 必须拒绝非法 page/index，并串行化目标 lineage
的 publication。

Truncate 是 soft truncate：缩小只隐藏超出新 count 的 page，不保证擦除 frame；
在覆盖前重新扩张可能看见旧 bytes。需要 zero-filled page 的调用者必须主动写入。
这不是 secure erase。

## 7. Runtime 与本地存储

Runtime 绑定 async execution、local storage 和 remote backend；同一 runtime clone
共享 instance/coordination。当前 native store 使用 Fjall，但私有 key encoding
不是 portable API。

Runtime 操作包括 tag/volume 管理，reader/writer，pull/push/status/snapshot，log
fetch，commit hash、page/checksum、hydrate/publish、volume checkout/diff/reset 和
storage GC。Adapter 可暴露子集，但不能改变已暴露语义。

本地 cache 缺远端 immutable data 不等于 snapshot 不存在；本地存在 orphan
segment 也不证明 repository 可达。

## 8. Hydration 与 publication

Hydration 让目标 snapshot 所需 commit/frame 在本地可用，可以 exact、按需或
cache。Exact hydration 必须拿到读取全部逻辑 page 所需内容，并校验 supplied
expected hash。Hydration 不是 worktree materialization，不能写 application file、
移动 `HEAD` 或改 index。Cache 可重建和淘汰。

Missing-page planning 可以只传本地没有的 page/frame，但最终 bytes 与校验必须
相同；部分 hydration 不能冒充 fully available。

Snapshot push 发布 remote 所需 immutable commit/segment。Immutable data 必须先于
让其可达的 mutable metadata 发布；相同 immutable key 重传必须 idempotent 或
content-equal。单独 storage push 不更新 repository branch，最终 ref CAS 由 Remote
Sync 规格负责。

## 9. 失败、取消与 pending publication

Local commit publication 从 reader 视角必须原子：只看到旧或新状态。Remote
push 在无法判断结果时保留 pending record。取消只保证 safe boundary；请求发出后
timeout/断线可能是确定拒绝、可能成功但 ACK 丢失，或 outcome unknown。实现必须
保留足够状态以 reconcile，不能盲目发布不同 successor；adapter 应映射成不同的
publication-unconfirmed/outcome-unknown error。

Corruption、hash mismatch、不连续历史和无法恢复的 missing frame 必须明确失败；
恢复不能制造 page 或静默移动 sync point。

## 10. Storage GC

Storage GC 从 supplied snapshot/volume root 追踪到 log、commit、segment、frame。
Repository 层把 ref、index、merge state、payload pointer 转成 storage root。GC
必须保留所有 root 和 pending recovery 所需数据；可以在安全 grace/coordination
边界后删除 unreachable data。Object GC 与 payload prune 是另外的 domain。

## 11. Conformance

至少测试：4096-byte page 与边界、per-log LSN、单/多 log snapshot overlay、stable
reader、writer read-your-writes、soft truncate、lazy/exact hydration、hash/missing
failure、idempotent immutable publication、pending outcome recovery、历史 volume
checkout 不改源，以及 reachability-safe GC。

当前证据位于 `crates/graft/src/rt/`、`crates/graft/src/local/`、snapshot/log/volume
model test、storage action test 和 repository SQLite snapshot integration test。

## 12. 兼容说明与限制

- 1.0 固定 4096-byte page。
- LSN 是 per-log logical counter，不是 hash/global clock。
- Soft truncate 不提供 secure erase。
- Fjall 是当前引擎，不是标准 interchange format。
- Hydration 可以 lazy；要求完整本地可用时必须调用有该 postcondition 的操作。
- Repository branch publication atomicity 属于 Remote Sync，不属于单独 storage push。
