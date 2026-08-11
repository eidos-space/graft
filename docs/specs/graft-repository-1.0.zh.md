# Graft Repository 1.0（中文参考）

状态：与当前实现对齐的规范草案
版本：1.0
发布日期：2026-08-11
规范语言：英文

## 摘要

本规格定义 Graft repository discovery、磁盘布局、canonical object、路径身份、
ref、index stage、tracked artifact、status、history、配置与 repository 维护。
它是 storage snapshot、diff、merge、remote sync 与 host adapter 共用的 identity
和 metadata 层。

本规格不定义 Fjall 私有 key layout、SQLite page storage、物理 worktree 替换、
remote wire transport 或 UI。

## 1. 范围与职责

Repository 层负责：

- repository format 的发现与校验；
- 规范化 repository-relative path identity；
- content-addressed blob、tree、commit 与 annotated tag；
- `HEAD`、branch、remote-tracking ref 与 tag ref；
- index stage、inventory、status 与 history metadata；
- repository config、ignore 与 track-root policy；
- object/payload 维护所使用的 reachability root。

Page/log storage 见 [Graft Storage and Snapshots
1.0](./graft-storage-snapshots-1.0.md)，逻辑比较见 [Graft Diff
1.0](./graft-diff-1.0.md)，merge state 见 [Graft Merge
1.0](./graft-merge-1.0.md)，物理投影见 [Graft Worktree Materialization
1.0](./graft-worktree-materialization-1.0.md)。

## 2. Repository discovery 与布局

Graft repository 是包含 `.graft` 的 worktree。`init` 必须 canonicalize 根路径、
创建布局、写默认配置，并让 unborn `HEAD` 指向默认 branch。`open` 必须校验
repository/object format。`discover` 从给定目录向父目录查找；对文件执行 discovery
时从其父目录开始。找不到或格式不支持时必须明确失败，不能静默 init。

当前持久格式：

```text
repository format version = 2
object format             = blake3
object envelope version   = 1
```

主要路径：

```text
.graft/config.toml
.graft/HEAD
.graft/MERGE_HEAD
.graft/ORIG_HEAD
.graft/refs/{heads,remotes,tags}/
.graft/logs/{HEAD,refs/}
.graft/objects/{,pack/}
.graft/store/{fjall,files}/
.graft/index/{state.toml,worktree.toml}
.graft/{locks,tmp}/
```

Cache/temp 文件只要可重建且不参与 object identity，就可以兼容增加。

Repository path 必须是规范化 UTF-8 相对路径，不能有空、`.`、`..` component，
不能指向 `.graft` 或逃逸 worktree。平台等价写法必须先归一成唯一 identity。

## 3. Canonical object 与 identity

### 3.1 Envelope 与 object ID

Canonical loose object 字节为：

```text
graft-object 1 <kind> <payload-length>\0<payload>
```

`kind` 是 `blob`、`tree`、`commit` 或 `tag`。Object ID 是整个 envelope 的
BLAKE3，表示为 64 个十六进制字符。读取必须校验 bytes 与请求 ID 一致；loose
store 用前两个字符 fanout。Writer 必须确定性输出；reader 可以读文档化 legacy
encoding，但重写时必须使用当前 canonical encoding。

### 3.2 Blob

| 格式 | 用途 | 约束 |
| --- | --- | --- |
| `sqlite-snapshot-v1` | SQLite snapshot descriptor | volume、page count、顺序 log range 与每个 LSN 的 commit hash |
| `file-blob-v2` | inline file | 当前 Base64 writer 格式 |
| `file-blob-v1` | legacy inline file | 兼容读取 Base58 |
| `large-file-pointer-v1` | external payload pointer | kind、BLAKE3 content hash、size |

SQLite descriptor 必须校验 page count、range 顺序/边界，以及每个 LSN 对应的
storage commit hash。它表示 canonical storage state，不是物理数据库。External
pointer 即使本地缺 bytes 仍然有效；真正需要 bytes 的操作必须报告缺失。

### 3.3 Tree、commit 与 tag

Tree entry 按 canonical path 排序且不能重复。当前 mode：

```text
100644  普通 inline/external file artifact
160000  SQLite snapshot entry
```

Commit 包含 root tree、顺序 parent、author/committer、Graft version、message、
SQLite table summary 与可选 path change count。Initial commit 无 parent，普通
commit 一个 parent，完成的 three-way merge 按 ours、theirs 顺序有两个 parent。

Lightweight tag 是直接 ref；annotated tag object 记录 target/type/name/tagger/message。
Tag 的创建、覆盖和删除不能静默替换无关目标。

## 4. HEAD、ref 与 revision

命名空间：

```text
refs/heads/<branch>
refs/remotes/<remote>/<branch>
refs/tags/<tag>
```

`HEAD` 可以 symbolic 指向本地 branch，也可以 detached 到 commit；首次 commit 前
unborn symbolic branch 合法。单个 ref 更新必须原子，并应更新对应 reflog。

安全 branch 删除要拒绝当前 branch 和尚未 merge 的 branch；force 可以跳过 merge
检查，但不能破坏 `HEAD`。Rename 当前 branch 时必须同步 symbolic `HEAD`。

Revision resolver 支持完整 ID、4–63 字符无歧义 hexadecimal 前缀、`HEAD`/`@`、
branch、remote-tracking ref、lightweight/annotated tag（peel 到 commit），以及 `~n`
first-parent、`^n` numbered-parent；`^0` 仍是当前 commit。歧义、缺 parent、非法
名称或要求 commit 时解析出非 commit，都必须失败。Ancestor/merge-base 查询只读。

## 5. Index 与 staging

Index 按 path、stage 排序：

| stage | 名称 | 含义 |
| --- | --- | --- |
| `0` | Normal | 下一 commit 的 staged result |
| `1` | Base | unresolved merge 的共同祖先 |
| `2` | Ours | 本地侧 |
| `3` | Theirs | merge target 侧 |

非 merge 状态每个 path 最多一个 Normal entry。冲突中缺少某一 stage 表示该侧
删除；resolved path 收敛为一个 Normal entry 或 staged deletion。存在 unresolved
stage 时 `commit` 必须失败。

Normal index 是 `HEAD` overlay：artifact/snapshot 表示 add/replace，空 entry 表示
delete。`commit` 消费精确 staged state，不能在 commit 时重抓更新后的物理文件。
Stage SQLite 时要捕获一致 standalone image，包括 committed WAL frame、不包括
uncommitted transaction，并可复用未改变的 4 KiB snapshot 内容。

Worktree observation 原子记录 dirty/deleted path，但只是输入与优化信号；需要保证
正确性时仍要验证当前文件。损坏或过期 observation 必须可重建。

## 6. Tracked file、SQLite 与 external payload

公开分类：

```text
path kind:  sqlite_database | text_file | binary_file
storage:    sqlite_snapshot | inline | external
```

不超过 threshold 的 UTF-8 文本通常 inline；binary、超阈值文本和匹配
`external_paths` 的 path 使用 external pointer；SQLite 使用 snapshot descriptor。

External bytes 按 content hash 存在 `.graft/store/files`。Payload status、fetch、
audit/repair、prune 必须区分 pointer reachability 与 bytes availability。Prune root
包括 index、local/remote ref 和 tag 的 reachable objects，不能删除 reachable
payload；hash mismatch 或无法恢复的缺失必须报告。

`track.default_roots` 与 `track.user_roots` 取规范化去重并集；空集允许所有可见
path，非空则限制 discovery/staging。`.graftignore` 与 `.gitignore` 共同参与 ignore；
ignore 不能静默 untrack 已跟踪 path。Inventory 要区分 tracked、untracked、ignored
和 tracked-but-now-ignored。

## 7. 配置契约

默认值包括 repository format 2、`blake3`、branch `main`、1 MiB inline text
threshold、空 external path/track root、开启物理 SQLite materialization，以及 merge
规格定义的 built-in resolver。

Generic config API 只接受：

```text
files.inline_text_threshold
files.external_paths
track.default_roots
track.user_roots
worktree.materialize_sqlite
merge.default_semantic_keys
merge.semantic_keys.<table>
merge.generated_columns.<table>
merge.internal_resolvers.<subject>
merge.schema_resolvers.<operation>
```

值必须 type-check，resolver 必须属于支持组合。Unset scalar 恢复默认；unset
per-table/per-subject override 则删除 override。Unknown key 必须失败。Remote 与
branch upstream 由专用操作管理；credential 不能写入 config。

## 8. Status、inventory 与 history

Status 比较 `HEAD`、index 与 worktree，报告 staged/unstaged/conflicted、path kind、
storage、Git-like code、branch/detached/unborn、upstream ahead/behind/diverged，以及
active merge 等 work-in-progress。Status 只读。Cache/incremental 实现必须在 config、
ref、index、ignore、tracked/untracked fingerprint 变化时失效，命中结果必须等价
full scan。

Inventory/ignore SDK API 应有界并可分页。显式 path 查询不能静默漏项。History
summary 可只读 commit metadata；details 与 changed paths 延迟 hydrate tree/blob。
分页在同一 repository state 下必须确定。Changed path 包括 add/delete/modify/exact
move；SQLite 按 snapshot identity，普通文件按完全相同内容配对，1.0 不定义
similarity rename。

## 9. Restore、reset 与维护

Reset 语义：

| mode | ref | index | worktree |
| --- | --- | --- | --- |
| soft | 移动 | 保留 | 保留 |
| mixed | 移动 | 重置到 target | 保留 |
| hard | 移动 | 重置到 target | 投影 target |

物理投影由 materialization 规格负责。Restore/reset 遇到 unresolved state 时必须
拒绝，除非操作明确规定如何替换该状态。

每次 object read 都要校验 canonical bytes 与请求 ID。当前 top-level `audit` 检查
tracked artifact/external payload，并可从 remote 修复可获取的 object/payload；当前
`gc` 追踪 SQLite storage root，不是 loose repository-object collector。

Payload prune 保留 active index 以及 local branch、remote-tracking ref、tag reachable
commit 引用的 payload。SQLite storage GC 还以 index snapshot、`HEAD`、branch、
merge/original head、remote ref 和 tag 为 root。这些 domain 不能
混用。1.0 没有删除 unreachable loose object 的 public command。

## 10. 原子性、并发与失败

当前有两类写入：`HEAD`、ref、config、worktree observation 与 external payload
replacement 使用 sibling temp + rename；loose object、index、`MERGE_HEAD/ORIG_HEAD`
仍直接写文件。Object read 校验 canonical bytes/ID，index/merge read 完整 parse，
因此 torn/corrupt 会被发现，但 1.0 **不承诺**这些 direct write crash-atomic。
Temp+rename helper 当前也没有 file/directory `fsync` durability 契约；reflog 在 ref
replacement 后单独 append，不是同一 transaction。

Higher-level commit 会先写所需 immutable content 再移动 ref；带 expected head/token
的操作遇 mismatch 必须 stale。Mutation 经过 command-service/storage lock，同一 SDK
session 还会串行，但 direct core user/独立进程仍要遵守 coordination。多文件操作
不是单一事务；record/temp/validation/recovery 能发现常见中断，power loss during
direct metadata write 仍可能需要从 reachable object/ref 或 remote 修复。恢复不能
制造 content 或丢弃冲突双方。

## 11. Conformance 要求

`GRAFT-Core-1.0` 至少要测试：canonical object vector、畸形 path/envelope/tree/
snapshot 拒绝、discovery/format 校验、unborn/attached/detached HEAD、expected-value
ref update、index stage、冲突时 commit 拒绝、一致 SQLite/file staging、ignore 与
track root、full 与 cached status 等价，以及 object read 校验和 reachability-safe
payload/storage maintenance。

当前证据主要位于 `crates/graft/src/repo/`、`crates/graft/src/repo/tests.rs`、
`crates/graft-sqlite/src/` 的 staging/command-service 测试和 adapter integration test。

## 12. 兼容说明与限制

- Reader 仍接受 `file-blob-v1`；writer 输出 `file-blob-v2`。
- Repository format 2 与 envelope 1 是持久契约；Fjall 私有 key 不是 portable API。
- Move detection 是 exact，不是 similarity-based。
- Status cache 可重建，不是 canonical state。
- Generic config API 有意不允许任意 TOML 编辑。
- 1.0 有 payload prune 与 SQLite storage GC，但没有 public loose-object GC。
- Loose object、index 与 merge record 读时校验，但写入尚非 crash-atomic/fsync-durable；
  reflog 与 ref replacement 也不是同一 transaction。
- 当前尚未输出机器可读 conformance/capability record。
