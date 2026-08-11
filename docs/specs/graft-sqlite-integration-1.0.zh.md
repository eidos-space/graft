# Graft SQLite Integration 1.0（中文参考）

状态：与当前实现对齐的规范草案
版本：1.0
发布日期：2026-08-11
规范语言：英文

## 摘要

本规格定义 Graft SQLite extension 与 live VFS data plane：配置/注册、把 DB 打开或
import 成 volume、page I/O、lock transition、snapshot freshness、与 repository
checkout 的协调、secondary file、SQLite error mapping 与 production PRAGMA boundary。

VFS 与物理 materialization 不同：VFS 从 Graft storage 实时服务 SQLite；物化把
snapshot 投影成给非 VFS application 使用的普通文件。

## 1. 架构

```text
SQLite (`vfs=graft`) -> GraftVfs / VolFile -> Runtime -> VolumeReader/Writer
CLI / SDK             -> RepositoryCommandService（独立 control plane）
```

VFS 负责 live file/lock/page/volume binding；repository service 负责 status/stage/
commit/history/branch/merge/remote。Production repository command 不能走 PRAGMA。

## 2. Extension config 与注册

Extension init 时从 host process 当前 working directory 读取 optional
`graft.toml`。字段：

| field | 含义 |
| --- | --- |
| `remote` | base runtime remote |
| `data_dir` | persistent local storage；缺省为 temporary |
| `log_file` | append tracing file，否则用 SQLite logger |
| `make_default` | 将 `graft` 注册为 default VFS |
| `autosync` | 可选非零秒数 |

Invalid config/path/runtime/registration 必须让 init 带 SQLite error 失败。Dynamic 与
static 都注册名 `graft`；dynamic permanent-load。多 connection load 时第一个 tracing
subscriber 生效，后续不能因 already initialized 而 panic。

## 3. Runtime 与 tag

规范化 main DB path 是 VFS tag。若 discover 到 repository，VFS 以其 storage dir
fork runtime，并按 canonical `.graft` cache；否则用 base runtime。Tag normalization
必须稳定且拒绝非法形式。`access` 在 tag 或 recognized physical SQLite 存在时为 true。

## 4. Main DB open

顺序：

1. tag 已绑定 volume，打开；
2. 否则存在 physical SQLite，import 新 volume 后绑定；
3. 否则 SQLite 请求 create，创建并绑定；
4. 否则 `SQLITE_CANTOPEN`。

Read-only 不能升级 writer；create 不能静默替换 incompatible file。

Import 要求 readable SQLite header、page size 4096、length 是 4096 整数倍、page
count 可表示。逐 page 验证写入新 volume，commit 后才发布 tag。失败不能暴露部分
import。它是 VFS bootstrap，不是 stage/hydration/materialization。

当前 importer 直接读 main database file，不运行 SQLite online backup，也不合并
`-wal`。若可能有 committed WAL frame，首次 VFS import 前调用者必须 close/checkpoint
普通 SQLite writer；这弱于 repository staging 的一致 WAL capture。Source physical
file 不会删除，也不会作为 live mirror 同步；tag 建立后 `vfs=graft` 优先打开 volume，
非 VFS application 打开普通文件可能看到旧 bytes 并产生分叉。

## 5. Secondary file

只有 `MainDb` 使用 VolFile。Journal/temp 等 secondary file 使用 isolated in-memory
file；delete 幂等 no-op，close 释放。不能把 WAL/journal/temp 名误绑为 durable tag；
这些 secondary bytes 不保证 crash 后存在。

## 6. Page I/O

File size 为 `4096 * page_count`。单次 read/write 不能跨 4096-byte page。Write 与
truncate 要 Reserved；truncate size 是 4096 整数倍并遵守 soft truncate。Storage
missing/error 不能伪装成 valid zero page。

Advertised characteristic：atomic through 4 KiB、powersafe overwrite、safe append、
sequential order。

SQLite page 1 的 file-change counter 与 version-valid-for 若是 full-page write 的唯一
变化，可以忽略以避免无意义 snapshot；读取时从 snapshot identity 合成 counter。
其他 page-1 byte 变化绝不能忽略；该 counter 不是 repository identity。

## 7. Lock/transaction state machine

```text
Idle -> Shared(reader) -> Reserved(writer)
                         -> Committing -> Shared(new reader)
Shared -> Idle
```

Pending/Exclusive 只在 Reserved 合法。

Idle->Shared refresh binding 并打开 stable reader。Shared->Reserved 要求 writable、
per-tag mutex 可用、reader snapshot 仍 latest、workspace coordinator 允许 writer。
Mutex/workspace 竞争为 `SQLITE_BUSY`；snapshot 过期为 `SQLITE_BUSY_SNAPSHOT`，SQLite
必须重启 transaction。

Reserved unlock-to-Shared 附加 pending message、commit writer、安装新 reader、释放
guard，再把 repository path 标 dirty。Dirty bookkeeping 可能在 durable SQLite commit
后失败，此时必须报告并由后续 status/stage 恢复，不能假装 volume commit 回滚。
Commit failure 后 SQLite unlock-to-Unlocked 释放 guard；非法 transition 必须失败。

## 8. Workspace coordination

一个 coordinator 排斥 live VFS writer 与 repository checkout/materialization：checkout
只在 writer count 0 时拿 exclusive flag；writer 在确认无 checkout 后 increment，并
再次检查关闭 race；release 恢复。拿不到返回 busy。它只是 in-process gate，不能
替代 materialization 规格的 application handle 与 filesystem lock。

## 9. PRAGMA surface

唯一 non-debug informational PRAGMA：

```sql
PRAGMA graft_version;
```

`graft_status/add/commit`、branch/merge/remote repository PRAGMA 已从 production
移除，应报错并指向 CLI/SDK。只有 feature-gated legacy test constructor 可启用。

`graft_debug_*` 当前覆盖 volume info/status/list/tags/snapshot/header、log/table log、
new/switch/clone/fork/checkout LSN/reset/message、fetch/pull/push/audit/hydrate/export、
raw LSN/commit/page/row diff。它们操作 storage volume/log/LSN，不是 repository
branch/commit，并且是不稳定 diagnostic，不能作产品 API。Unknown PRAGMA 不能近似执行。

## 10. Error mapping

| condition | SQLite result |
| --- | --- |
| unknown PRAGMA | `SQLITE_NOTFOUND` |
| missing tag | `SQLITE_CANTOPEN` |
| writer/workspace contention | `SQLITE_BUSY` |
| stale snapshot/concurrent write | `SQLITE_BUSY_SNAPSHOT` |
| cancellation | `SQLITE_INTERRUPT` |
| storage/remote I/O | `SQLITE_IOERR` |
| invalid transition/recovery/divergence | `SQLITE_INTERNAL` 或明确 PRAGMA error |

Error context 要保留具体 code、redact credential，且不能在 storage commit 已完成时
谎称 transaction rollback。

## 11. Crash、close 与恢复

Shared reader immutable；成功 writer commit 发布一个新 snapshot。Commit 前 crash
丢 in-memory secondary/uncommitted state；commit 后 crash 可能需要重发现 dirty path。
Close 释放 lock-manager reference；最后引用可移除 per-tag lock。Version 1.0 不承诺
delete-on-close 的 durable volume deletion。

另进程看不到 VFS object 不代表 ordinary-file checkout 安全；materializing operation
仍需关闭/重开 application connection。

## 12. Conformance

至少测试 config/dynamic/static registration、tag/runtime、open 四分支、4096 validation、
无 partial tag、in-memory secondary、I/O/page1、全部 lock transition、busy 与
busy-snapshot、commit/dirty/failure recovery、checkout-writer race、production PRAGMA
移除与 error mapping。

当前证据位于 `crates/graft-sqlite/src/vfs.rs`、`file/vol_file.rs`、PRAGMA/parser、
workspace/checkout test 与 `crates/graft-ext` dynamic/static test。

## 13. 已知限制

- Live VFS import/write 要求 4096-byte SQLite page。
- 首次 physical VFS import 只读 main file，不 reconcile WAL，也不维护 source live mirror。
- Secondary file 在内存，不保证 crash persistence。
- `graft_debug_*` 不是稳定应用 API。
- Coordinator 只在进程内；跨进程物理安全由 materialization 协议负责。
- Dirty marking 可在 durable commit 后失败，由 inspection 恢复，不回滚 SQLite commit。
