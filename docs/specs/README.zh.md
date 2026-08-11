# Graft 规格

状态：规格索引
规格套件版本：1.0
规范语言：英文

本目录是与 Graft 当前实现对齐的契约集合，记录 repository core、page store、
SQLite integration、remote protocol、CLI、SDK 与浏览器/WASM host 共同对外呈现的
行为。结构参考 Eidos Runtime 与 Adapter 规格：每类语义只有一个规范 owner，
adapter 只映射行为，不重新定义行为。

本套规格约束稳定契约与当前兼容边界，不冻结没有可观察影响的私有 Rust 类型、
存储引擎内部 key layout、UI 外观或实现技巧。

## 文档

| 领域 | 规范性文档 | 中文参考 | 主要 owner |
| --- | --- | --- | --- |
| Repository 与对象模型 | [Graft Repository 1.0](./graft-repository-1.0.md) | [中文](./graft-repository-1.0.zh.md) | `graft::repo` |
| Page storage 与 snapshot | [Graft Storage and Snapshots 1.0](./graft-storage-snapshots-1.0.md) | [中文](./graft-storage-snapshots-1.0.zh.md) | `graft` runtime/storage |
| Diff 与历史检查 | [Graft Diff 1.0](./graft-diff-1.0.md) | [中文](./graft-diff-1.0.zh.md) | repository diff + `graft-sqlite` row diff |
| Merge 与冲突恢复 | [Graft Merge 1.0](./graft-merge-1.0.md) | [中文](./graft-merge-1.0.zh.md) | repository merge + SQLite row merge |
| Remote 与同步 | [Graft Remote Sync 1.0](./graft-remote-sync-1.0.md) | [中文](./graft-remote-sync-1.0.zh.md) | repository sync + remote protocol |
| Runtime、CLI 与 SDK adapter | [Graft Runtime and Adapters 1.0](./graft-runtime-adapters-1.0.md) | [中文](./graft-runtime-adapters-1.0.zh.md) | command service + SDK adapter |
| SQLite extension 与 VFS | [Graft SQLite Integration 1.0](./graft-sqlite-integration-1.0.md) | [中文](./graft-sqlite-integration-1.0.zh.md) | `graft-sqlite` + `graft-ext` |
| 物理 worktree 投影 | [Graft Worktree Materialization 1.0](./graft-worktree-materialization-1.0.md) | [中文](./graft-worktree-materialization-1.0.zh.md) | SQLite worktree adapter |

## 阅读顺序

Repository 规格定义 identity、object、ref 与 index；Storage 规格定义 SQLite
snapshot object 引用的 page/log 基础；Diff 与 Merge 消费两者；Remote Sync
传输 immutable 与 mutable state；Runtime/Adapter 将操作暴露给 host。SQLite
Integration 与 Worktree Materialization 是两种不同投影：前者通过 VFS 提供实时
SQLite I/O，后者在物理 worktree 中创建或替换普通文件。

```text
Repository objects / refs / index
          |                 |
          v                 v
Storage snapshots         Diff + Merge
          \                 /
           v               v
             Remote sync
                  |
          Runtime / adapters
             /           \
      SQLite VFS     Physical worktree
```

## 规格状态与语言

英文文档是规范来源；中文文档按章节对齐，仅作信息性参考，不能改变英文含义。
所有 1.0 文档都是**与当前实现对齐的规范草案**：要求必须有当前源码与测试证据；
已知缺口和兼容边界会明确列出，不会伪装成更强保证。

英文文档中以全大写出现的 **MUST**、**MUST NOT**、**REQUIRED**、**SHALL**、
**SHALL NOT**、**SHOULD**、**SHOULD NOT**、**RECOMMENDED**、
**NOT RECOMMENDED**、**MAY** 和 **OPTIONAL**，按 BCP 14 解释。

## 职责归属与单一事实来源

| 可观察行为 | 规范 owner |
| --- | --- |
| Repository discovery、规范化路径、object、commit、ref、index、status、history | Repository |
| 4 KiB page、log、LSN、volume、snapshot、hydration、storage GC | Storage and Snapshots |
| path/content/row/schema/opaque 比较与有界检查 | Diff |
| topology、merge state、conflict、resolution、continue/abort | Merge |
| Remote URI/backend、wire protocol、fetch/push/pull/clone 与 publication | Remote Sync |
| 操作名、session 生命周期、串行化、取消、错误与 host 映射 | Runtime and Adapters |
| 实时 SQLite VFS、extension 注册、锁状态与生产 PRAGMA | SQLite Integration |
| 普通 SQLite 文件、WAL/sidecar、replacement lock 与恢复 | Worktree Materialization |
| Playground 布局与视觉交互 | UI 实现；不属于本套规格 |

上层规格可以摘要下层规则，但必须链接到 owner，不能重新定义。源码注释、
README、guide 与旧设计记录属于实现证据，不会修改本套规格。若旧文档与当前
代码、测试和本套规格冲突，以版本化规格表达的目标行为为准，并将差异视为
实现或文档 conformance 问题。

## 跨规格不变量

所有 conforming profile 都必须保持：

1. Repository path 是规范化 UTF-8 相对路径，不能访问 `.graft` 内部。
2. Object 与 snapshot identity 由内容导出；mutable ref 不会改变 immutable object。
3. Stage、commit、hydrate、临时 diff 数据库、export、实时 VFS 访问与物理
   worktree materialization 是不同操作。
4. 只读 plan/inspection 不会静默移动 ref、替换 worktree 或解决冲突。
5. 会改变状态的操作要检测 stale state，以可重试错误暴露竞争，不能静默覆盖。
6. Remote credential 是显式 adapter 输入，不能写入 config、URL、cache 或结果。
7. Browser/WASM host 必须披露原生能力缺口，不能用 mock 作为 core conformance 证据。

## Conformance profile

```text
GRAFT-Core-1.0       repository、object、ref、index、storage、diff 与 merge
GRAFT-Remote-1.0     remote protocol 与同步
GRAFT-CLI-1.0        基于 command service 的 CLI 映射
GRAFT-SDK-1.0        retained Rust/Node repository session
GRAFT-Browser-1.0    WASM/OPFS host 组合与能力披露
GRAFT-VFS-1.0        实时 SQLite VFS 与 extension surface
GRAFT-Worktree-1.0   普通文件物化与恢复
```

实现必须分别声明 profile。Profile 描述行为，不绑定某个 crate 或语言。当前仓库
尚未输出机器可读 capability record，因此这些名称是 conformance 目标，不是
release claim。

## 实现证据索引

| 规格 | 主要源码 | 可执行证据 |
| --- | --- | --- |
| Repository | `crates/graft/src/repo.rs` 与 `repo/` | object/ref/index/history/inventory test |
| Storage | `core/`、`snapshot.rs`、`volume.rs`、`rt/`、`local/` | runtime/action/hydration/GC test |
| Diff | repository diff/history 与 `row_level_diff.rs` | rowid/PK/schema/opaque/bounded/SDK test |
| Merge | core merge 与 SQLite row merge/output | topology/conflict/resolution/reopen/browser fixture |
| Remote | repository sync、remote/runtime action、`packages/graft-remote` | Rust 与 framework/Hono/Cloudflare protocol test |
| Runtime/Adapter | command service、Rust/Node SDK、web worker | lifecycle/cache/cancel/error/contract/Playwright |
| SQLite Integration | VFS/VolFile/PRAGMA 与 `graft-ext` | lock/import/error/PRAGMA/dynamic/static test |
| Materialization | checkout/snapshot/merge worktree path | WAL/replacement/recovery/gate integration test |

表格是证据导航，不按文件划分职责；跨多个 crate 的行为仍只有前文列出的单一
规范 owner。

## 当前兼容性与技术债登记

以下内容是 1.0 baseline 明确披露的当前限制，adapter 不能静默补成更强保证：

| 当前限制或文档漂移 | 规范 owner |
| --- | --- |
| Loose object、index 与 merge record 的直接写入会在读取时校验，但不具备 crash-atomic 或 fsync durability 保证 | [Repository](./graft-repository-1.0.zh.md#10-原子性并发与失败) |
| 普通非 key BLOB row value 沿用裸十六进制 JSON 字符串，需要 schema context 才能与 TEXT 区分 | [Diff](./graft-diff-1.0.zh.md#12-已知限制) |
| VFS 首次从物理文件导入时只读取 main database，不会合并未 checkpoint 的 WAL | [SQLite Integration](./graft-sqlite-integration-1.0.zh.md#13-已知限制) |
| Browser/WASM 当前不实现 remote sync，也不能加载 Node native addon | [Runtime and Adapters](./graft-runtime-adapters-1.0.zh.md#12-browserwasm-profile) |

若要把任一项从已披露限制升级为支持保证，必须提供实现证据并更新 conformance；
只删除警告不会改变实际行为。

## 变更策略

兼容澄清可以增加示例、测试向量、限制或链接。若改变 persisted format、object
identity、merge 结果、wire protocol、操作后置条件、失败原子性或某操作是否可
替换 worktree，必须升级版本或声明兼容扩展。没有可观察变化的私有重构不要求
修改规格。
