# Graft

**面向 SQLite 应用状态的版本控制。**

[在线体验](https://graft.eidos.space/playground/) ·
[文档](https://graft.eidos.space/zh/) ·
[版本发布](https://github.com/eidos-space/graft/releases) ·
[English](./README.md)

Graft 将 SQLite 数据库和应用自有文件记录为一个完整的应用状态。它提供提交、
分支、行级差异、合并、恢复和远程同步能力，无需自定义 SQLite VFS。

## 为什么使用 Graft？

应用状态通常同时存在于数据库和周边文件中：

```text
app-data/
  data.sqlite
  settings.json
  attachments/
```

SQLite 能保证数据库事务一致，却不会为整个目录保留版本。Git 可以记录文件，
但通常只能把 SQLite 视为不透明的二进制文件。Graft 同时处理两者：

- 为 SQLite 数据库和关联文件创建一致快照
- 提供 SQLite 表级、行级差异与合并
- 提供类似 Git 的提交、分支、标签和恢复
- 提供结构化 CLI 输出、嵌入式 SDK 与远程同步

```text
SQLite 管事务，Graft 管历史。
```

## 体验 Graft

[Graft Playground](https://graft.eidos.space/playground/) 会在浏览器中运行
真实的 Graft CLI。无需安装即可体验提交、分支、行级差异与冲突处理。

在 macOS 或 Linux 安装最新版本：

```sh
curl -fsSL https://graft.eidos.space/install.sh | sh
```

创建仓库并保存一个 SQLite 数据库：

```sh
mkdir graft-demo
cd graft-demo

graft init
graft sql --db data.sqlite \
  "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT NOT NULL);"
graft sql --db data.sqlite \
  "INSERT INTO notes(body) VALUES ('first note');"
graft add --all
graft commit -m "Save initial state"
graft log
```

继续阅读 [CLI 快速开始](https://graft.eidos.space/zh/docs/quickstart/cli/)。

## 选择集成方式

| 集成方式 | 适用场景 |
| --- | --- |
| [CLI](https://graft.eidos.space/zh/docs/quickstart/cli/) | 评估、脚本、智能体与一次性命令 |
| [结构化 JSON 输出](https://graft.eidos.space/zh/docs/reference/json-output/) | 稳定的应用与自动化边界 |
| [Node.js 与 Electron SDK](https://graft.eidos.space/zh/docs/sdk/) | 长驻的进程内仓库会话 |
| [远程服务包](https://graft.eidos.space/zh/docs/remotes/) | 托管 Graft HTTP 远程协议 |

安装 Node.js 与 Electron SDK：

```sh
pnpm add @eidos.space/graft
```

## 谁在使用 Graft？

### Eidos Lite

[Eidos Lite](https://eidos.space/zh/download#eidos-lite) 是面向 `.eidos`
多维表格与普通文件的本地优先桌面应用。它使用 Graft：

- 将整个本地工作空间记录为一个版本
- 保存前展示文件变化与 SQLite 行级变化
- 查看和恢复旧版本，同时保留已有历史
- 通过可选的 Eidos Sync 同步工作空间历史

集成代码位于开源的
[Eidos 仓库](https://github.com/mayneyao/eidos/tree/dev/apps/eidos-lite-desktop)。

如果你的产品也在使用 Graft，欢迎提交 pull request 添加到这里。

## 核心模型

1. 应用照常写入普通 SQLite 数据库和文件。
2. `graft add` 捕获一致快照并暂存相关路径。
3. `graft commit` 将它们记录为一个应用状态。
4. 差异、分支、合并、恢复与同步都基于这套历史运行。

仓库历史保存在普通工作树旁的 `.graft/` 中。应用可以继续使用原有的
SQLite 库与文件 API。

## 项目状态

Graft 目前仍是实验性项目。CLI、结构化输出、Node.js/Electron SDK 和远程协议
是受支持的集成界面；存储布局与内部 Rust 模块仍属于实现细节。

Graft 是 SQLite 事务的补充，不替代应用授权、实时复制或用于源码管理的 Git。

## 开发

完整开发流程见 [CONTRIBUTING.md](./CONTRIBUTING.md)。

```sh
cargo check --workspace --all-targets
just test

pnpm check:remote
pnpm test:remote
```

与实现对齐的规范位于 [`docs/specs`](./docs/specs)。

## 项目来源

Graft 最初从 [orbitinghail/graft](https://github.com/orbitinghail/graft)
分叉而来。原实现中的事务型存储引擎现已成为本项目 SQLite 存储层的基础。
本仓库现已独立维护，不再保留 GitHub fork 关系。

## 许可证

你可以选择 [Apache License 2.0](./LICENSE-APACHE) 或
[MIT License](./LICENSE-MIT)。
