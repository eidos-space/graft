> [!NOTE]
> Graft 最初从
> [orbitinghail/graft](https://github.com/orbitinghail/graft) 分叉而来。原实现中的
> 事务型存储引擎现已成为本项目 SQLite 存储层的基础。本仓库现已脱离 GitHub fork
> 关系并独立维护。

[English](./README.md) · **简体中文**

# Graft

**面向 SQLite 应用状态的版本控制。**

[在线体验](https://graft.eidos.space/playground/) ·
[文档](https://graft.eidos.space/zh/) ·
[版本发布](https://github.com/eidos-space/graft/releases) ·
[CLI 参考](https://graft.eidos.space/zh/docs/reference/cli/)

Graft 为 SQLite 数据库和应用自有文件建立一套连贯的版本历史。它提供类似 Git
的提交、分支、行级 diff、合并、恢复和远程同步能力。默认工作流直接使用普通的
SQLite 文件和现有 SQLite 库，无需自定义 VFS。

## 为什么使用 Graft？

应用状态很少只存在于一个表或一个文件中：

```text
app-data/
  data.sqlite
  settings.json
  attachments/
    avatar.png
    invoice.pdf
```

SQLite 能保证每个数据库事务的一致性，但不会为整个目录提供版本历史。Git
可以跟踪这个目录，却通常只能把 SQLite 数据库视为不透明的二进制文件。

Graft 将数据库及其关联文件记录在同一个版本中：

| 需求 | Graft 提供的能力 |
| --- | --- |
| 审查数据库变化 | SQLite 表级和行级 diff |
| 增量保存数据库历史 | 复用未变化的 4 KiB 数据块，只追加变化的数据，而不是每个版本都复制整个数据库 |
| 保存一致的应用状态 | 用一次提交同时记录数据库及其关联文件 |
| 安全地进行实验 | 分支、标签、恢复，以及对兼容变化的自动合并 |
| 在应用中处理冲突 | 结构化冲突信息和 JSON 输出 |
| 在设备之间同步历史 | 文件系统、S3 兼容存储和 HTTP 远程仓库 |
| 嵌入仓库工作流 | 支持长驻会话的 Node.js/Electron SDK |

Graft 适用于本地优先（local-first）应用、AI 辅助编辑、用户可见的版本历史、
检查点、变更审查，以及数据库行会引用 SQLite 之外文件的应用。

Graft 不能替代 SQLite 事务、应用授权、实时查询复制或用于源码管理的 Git。

## 体验 Graft

### 在浏览器中

[Graft Playground](https://graft.eidos.space/playground/) 通过 WebAssembly
运行真实的 Graft CLI，并将所有内容保存在浏览器中。无需安装任何软件，即可快速
体验提交、分支、SQLite 行级 diff 和冲突处理。

### 在本地

安装 Graft v0.13.0：

```sh
curl -fsSL https://graft.eidos.space/install.sh \
  | GRAFT_VERSION=0.13.0 sh

graft --version
```

移除 `GRAFT_VERSION=0.13.0` 即可安装最新发布版本。也可以从
[Releases](https://github.com/eidos-space/graft/releases) 下载预构建文件。

创建一个同时包含 SQLite 数据库和应用自有文件的仓库：

```sh
mkdir graft-demo
cd graft-demo

graft init
graft sql --db data.sqlite \
  "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT NOT NULL);"
graft sql --db data.sqlite \
  "INSERT INTO notes(body) VALUES ('first note');"

mkdir attachments
printf 'hello\n' > attachments/readme.txt

graft status
graft add --all
graft commit -m "Save initial app state"
```

再次修改数据库，并以行而不是二进制字节的形式查看变化：

```sh
graft sql --db data.sqlite \
  "INSERT INTO notes(body) VALUES ('second note');"

graft add data.sqlite
graft commit -m "Add second note"

graft diff --rows HEAD~1 HEAD data.sqlite
graft log
```

现在，数据库及其关联文件已经拥有两个可以恢复的版本。继续阅读
[CLI 快速上手](https://graft.eidos.space/zh/docs/quickstart/cli/)，体验分支、合并和恢复。

## 工作原理

Graft 会在应用的普通工作树中创建 `.graft/` 目录：

```text
app-data/
  data.sqlite          普通 SQLite 文件
  settings.json        普通应用文件
  attachments/
  .graft/              历史、暂存区、refs、对象和 payload 缓存
```

基本工作流与常见版本控制操作相似：

1. 应用照常写入 SQLite 数据库和其他文件。
2. `graft add` 捕获 SQLite 已提交状态的一致快照，并暂存相关文件。
3. `graft commit` 将这些路径记录为一个应用状态。
4. diff、分支、合并、恢复和同步都基于这套历史运行。

对于物理 SQLite 文件，暂存操作会包含已提交的 WAL frames，无需手动执行
checkpoint。Graft 以 4 KiB 存储块比较捕获的数据库镜像，并复用未变化的数据。

会改变检出状态的命令会将数据库快照和文件重新物化到工作树。在执行
`switch`、`checkout`、`restore`、`pull`、完成合并或 `reset --hard` 之前，
请关闭长时间持有的 SQLite 连接。如果其他写入者仍占用数据库，Graft 会拒绝替换它。

## 接入现有 SQLite 应用

Graft CLI 与 `sqlite3` 或应用中的 SQLite 库使用同一个物理数据库文件：

```sh
sqlite3 data.sqlite \
  "INSERT INTO notes(body) VALUES ('written by sqlite3');"

graft add data.sqlite
graft commit -m "Import SQLite transaction"
```

这个工作流不需要自定义 VFS。使用 `graft add <path>` 添加其他应用自有路径，
或使用 `graft add --all` 暂存整个工作树。

## 选择集成方式

除非应用需要更专门的集成边界，否则建议从 CLI 开始。

| 集成方式 | 适用场景 |
| --- | --- |
| [CLI](https://graft.eidos.space/zh/docs/quickstart/cli/) | 评估 Graft、编写脚本或执行一次性命令 |
| CLI + [JSON 输出](https://graft.eidos.space/zh/docs/reference/json-output/) | 应用或 agent 需要结构化的状态、diff、历史、冲突或同步结果 |
| [Node.js/Electron SDK](https://graft.eidos.space/zh/docs/guides/node-electron-sdk/) | Node.js 或 Electron 应用需要长驻的进程内仓库会话 |
| [SQLite 扩展](https://graft.eidos.space/zh/docs/quickstart/sqlite-extension/) | 应用希望通过 `vfs=graft` 直接存储实时 SQLite 页面 |
| [远程服务组件](https://graft.eidos.space/zh/docs/guides/http-remote/) | 希望托管 Graft HTTP 远程协议 |

安装 Node.js/Electron 常驻 SDK：

```sh
pnpm add @eidos.space/graft
```

## 常用工作流

| 工作流 | 命令 |
| --- | --- |
| 查看状态 | `status`、`log`、`show`、`diff --rows` |
| 记录状态 | `add`、`rm`、`commit` |
| 在历史中移动 | `checkout`、`restore`、`export`、`reset` |
| 使用分支 | `branch`、`switch`、`merge`、`conflicts`、`resolve` |
| 同步 | `remote`、`ls-remote`、`fetch`、`pull`、`push` |
| 维护存储 | `audit`、`gc`、`payload` |

完整参数、JSON schema 和示例请参阅
[CLI 参考](https://graft.eidos.space/zh/docs/reference/cli/)。

## 远程仓库

Graft 支持本地文件系统、S3、S3 兼容对象存储和 Graft HTTP 远程仓库：

```text
fs:///absolute/path
s3://bucket/prefix
s3_compatible://bucket/prefix?endpoint=https://...
https://host/namespace/repository
graft+http://127.0.0.1:8787/namespace/repository
```

```sh
export GRAFT_REMOTE_TOKEN='grt_...'

graft remote add origin https://example.com/acme/archive
graft push origin main
graft pull origin main
```

Bearer token 从环境变量读取，不会保存在远程 URL 中。详细说明请参阅
[远程同步](https://graft.eidos.space/zh/docs/guides/sync-remotes/)和
[HTTP 远程仓库](https://graft.eidos.space/zh/docs/guides/http-remote/)。

## 项目状态

Graft 目前仍是实验性项目。CLI、仓库配置、JSON 输出、Node.js/Electron SDK
和远程服务协议是预期对外使用的集成界面。存储布局、对象序列化、调试 PRAGMA
和内部 Rust 模块边界属于实现细节，未来可能发生变化。

## 文档

- [什么是 Graft？](https://graft.eidos.space/zh/docs/overview/what-is-graft/)
- [CLI 快速上手](https://graft.eidos.space/zh/docs/quickstart/cli/)
- [仓库模型](https://graft.eidos.space/zh/docs/concepts/repository-model/)
- [SQLite 快照](https://graft.eidos.space/zh/docs/concepts/sqlite-snapshots/)
- [跟踪数据库和文件](https://graft.eidos.space/zh/docs/guides/track-databases-and-files/)
- [比较行和文件](https://graft.eidos.space/zh/docs/guides/diff-rows-and-files/)
- [处理合并冲突](https://graft.eidos.space/zh/docs/guides/merge-conflicts/)
- [Node.js 和 Electron SDK](https://graft.eidos.space/zh/docs/guides/node-electron-sdk/)
- [CLI 参考](https://graft.eidos.space/zh/docs/reference/cli/)
- [性能基准](./crates/graft-bench/README.md)

## 参与贡献

开发流程和编码规范请参阅 [CONTRIBUTING.md](./CONTRIBUTING.md)。

```sh
just test
cargo check
cargo fmt
cargo clippy
```

## 许可证

你可以选择以下任一许可证：

- Apache License, Version 2.0（[LICENSE-APACHE](./LICENSE-APACHE)）
- MIT License（[LICENSE-MIT](./LICENSE-MIT)）
