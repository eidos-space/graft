# Graft

**为 SQLite 数据库与应用文件记录版本历史。**

[在线体验](https://graft.eidos.space/playground/) ·
[文档](https://graft.eidos.space/zh/) ·
[版本发布](https://github.com/eidos-space/graft/releases) ·
[English](./README.md)

Graft 为应用的 SQLite 数据库和相关文件记录版本。你可以查看行变化、恢复已记录
状态、合并受支持的分支修改，并把历史传到远端。应用继续使用普通 SQLite 连接
和文件 API。

## Graft 记录什么？

假设 `app.sqlite` 中的笔记引用 `attachments/report.pdf`，而 `settings.json`
控制应用如何显示它。你可以把选定的版本记录在同一次提交中，再回到这个组合。

Graft 理解受支持的 SQLite 内容：

- 捕获一致快照，包括 WAL 中已经提交的内容
- 通过内容寻址的数据块复用不同版本中未变化的 SQLite 内容
- 提供表结构、表和行级差异与合并
- 提供类似 Git 的提交、分支、标签、恢复与远端同步

```text
app-data/
├── app.sqlite
├── settings.json
└── attachments/
    └── report.pdf
```

共同提交不会在这些文件之间建立事务。如果它们必须对应同一个检查点，应用需要
协调写入。

Graft 采用类似 Git 的工作流，并使用独立的仓库格式与远端机制。

## 体验 Graft

[Graft Playground](https://graft.eidos.space/playground/) 在 OPFS 工作区中运行
浏览器版 CLI。无需安装即可练习本地提交、分支、行级差异与冲突处理。浏览器宿主
不提供原生远端同步能力。

在 macOS 或 Linux 安装最新版本：

```sh
curl -fsSL https://graft.eidos.space/install.sh | sh
```

创建仓库并保存一个 SQLite 数据库：

```sh
mkdir graft-demo
cd graft-demo

graft init
graft sql --db app.sqlite \
  "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT NOT NULL);"
graft sql --db app.sqlite \
  "INSERT INTO notes(body) VALUES ('first note');"
graft add app.sqlite
graft commit -m "Save initial state"
graft log
graft sql --db app.sqlite "SELECT id, body FROM notes;"
```

查询返回第一条笔记，历史中包含刚记录的版本。

继续阅读 [CLI 快速开始](https://graft.eidos.space/zh/docs/quickstart/cli/)。

在本地浏览器中打开已有仓库：

```sh
graft .
```

React 界面将文件内容和 diff 与目录树并排显示，通过 **All files**、**Changes**、
**History** 切换。你可以查看文本与 SQLite 行级变更、浏览当前数据库表，并编辑
较小的 UTF-8 文本文件。HTML 和 SVG 使用源码视图；工作区的位图、音频、视频
和 PDF 提供专门预览。
保存只修改工作区，暂存与提交仍通过 CLI 完成。服务仅监听
本机，按 Ctrl+C 停止。使用 `graft ui . --no-open --port 4178` 可指定端口并只打印
地址，不自动打开浏览器。

## 选择集成方式

| 集成方式 | 适用场景 |
| --- | --- |
| [CLI](https://graft.eidos.space/zh/docs/quickstart/cli/) | 评估、脚本、智能体与一次性命令 |
| [结构化 JSON 输出](https://graft.eidos.space/zh/docs/reference/json-output/) | 稳定的应用与自动化边界 |
| [Node.js SDK](https://graft.eidos.space/zh/docs/sdk/) | 长驻的进程内仓库会话 |
| [远程服务包](https://graft.eidos.space/zh/docs/remotes/) | 托管 Graft HTTP 远程协议 |

安装 Node.js SDK：

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

## 保存、暂存与记录

| 动作 | 改变什么 |
| --- | --- |
| SQLite 事务或文件写入 | 工作区中的当前应用文件 |
| `graft add` | 暂存区中选定的快照与文件字节 |
| `graft commit` | 本地历史，使用已暂存内容 |
| `graft push` | 远端历史，使用已记录提交 |
| 恢复或切换 | 工作区文件，使用已记录内容 |

如果在 `add` 之后插入新笔记，`commit` 仍记录之前的快照。新笔记留在工作区，
属于未暂存变化。暂存并提交它之前，push 不会把它作为新版本上传。

仓库历史保存在普通工作树旁的 `.graft/` 中。应用可以继续使用原有的
SQLite 库与文件 API。

暂存会捕获已提交的 WAL 内容，无需手动 checkpoint。恢复和分支切换可能替换
数据库文件，执行前应关闭受影响的应用连接。

[架构导读](https://graft.eidos.space/zh/docs/graft-book/)解释这些边界。
[动手练习](https://graft.eidos.space/zh/docs/graft-book/hands-on-lab/)让你先预测，
再执行命令，并用 SQL 查询检查结果。

## 项目状态

Graft 目前仍是实验性项目。CLI、结构化输出、Node.js SDK 和远程协议
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
