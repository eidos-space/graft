# Graft SQLite Page Delta 1.0（中文参考）

状态：与实现对齐的规范草案
规范语言：英文
Conformance profile：`GRAFT-Delta-1.0`

## 1. 范围

本规格定义两个精确、一致的 SQLite image 之间可移植的 `GRAFTD01` delta，
包括二进制格式、校验规则，以及与 repository 无关的 create、inspect、apply
操作。

它只是固定 page 的传输优化，不是 SQLite 逻辑 changeset、通用二进制 diff、
merge 格式，也不替代 Graft repository history。

## 2. 术语

- **Base**：应用 delta 时必须精确匹配的 SQLite image。
- **Target**：成功应用后得到的精确 SQLite image。
- **Page**：一个 4 KiB 传输块，从 1 开始编号。
- **Changed page**：与 base 对应 page 的精确字节不同，或位于 base 末尾之后的
  target page。
- **Consistent image**：包含 SQLite 可见已提交状态（含已提交 WAL 内容）的独立
  main-database image。

## 3. 格式标识与媒体类型

格式名为 `graft-sqlite-page-delta-v1`，八字节 magic 是 ASCII `GRAFTD01`。
网络 adapter 可以使用 `application/vnd.eidos.sqlite-page-delta`，媒体类型不会
改变格式语义。

## 4. 整数与摘要编码

所有整数都是 little-endian 无符号整数。SHA-256 字段保存 32 个原始摘要字节，
不是十六进制文本。下文 offset 从 0 开始。

## 5. 固定 header

Version 1 的 header 长 104 字节：

| Offset | 字节 | 字段 | Version 1 值或含义 |
| ---: | ---: | --- | --- |
| 0 | 8 | magic | `GRAFTD01` |
| 8 | 4 | header bytes | `104` |
| 12 | 4 | flags | `0` |
| 16 | 4 | page bytes | `4096` |
| 20 | 4 | changed page count | entry 数量 |
| 24 | 8 | base bytes | base image 精确长度 |
| 32 | 8 | target bytes | target image 精确长度 |
| 40 | 32 | base SHA-256 | base 精确字节摘要 |
| 72 | 32 | target SHA-256 | target 精确字节摘要 |

Base 和 target 长度必须是非零的 4096 整数倍；changed-page 数不能超过 target page
数。Version 1 reader 必须拒绝非 104 的 header 长度、非零 flags 或非 4096 的
page size。

## 6. Page entry

Header 后必须恰好有 `changed page count` 个 entry，每个 entry 是：

| 字节 | 字段 |
| ---: | --- |
| 4 | 从 1 开始的 target page number |
| 4096 | target page 的精确字节 |

Page number 必须严格递增、唯一、非零且不超过 `target bytes / 4096`。Base 末尾
之后的每一个 target page 都必须有 entry。Target 缩短只由 `target bytes` 表达，
被删除的尾部 page 不写 tombstone。

Delta 精确长度为：

```text
104 + changed_page_count * (4 + 4096)
```

Reader 必须拒绝不一致的物理长度。

## 7. Create 操作

符合规格的 creator 必须：

1. 为两个 SQLite 输入取得一致的独立 image；
2. 对每个 image 的全部精确字节计算 SHA-256；
3. 按精确字节比较 target 与 base page；
4. 按 page number 递增写入 entry；
5. 只创建新输出，不能覆盖；
6. 失败时删除不完整输出。

Creator 要报告 delta 是否比 target 更小，但不能在不划算时静默改写为完整 target。

## 8. Inspect 操作

Inspect 不需要 base，但必须校验固定 header、文件长度和全部 page-number 约束，
并返回内嵌长度、摘要、page 数及 delta 是否更小。Inspect 不证明 base 一定存在，
也不证明 target 一定能完成物化。

## 9. Apply 操作

符合规格的 applier 必须：

1. 取得一致的独立 base image；
2. 要求 base 精确长度与 SHA-256 匹配 header；
3. 将 base page 与替换 entry 流式写入 create-new 输出；
4. 按 target 精确长度截短或扩展；
5. 要求所有新增 page 都存在于 delta；
6. 用 target 摘要校验完整输出；
7. 任意失败时删除不完整输出。

Applier 不能把 base mismatch 降级成 best-effort，也不能覆盖已有输出。

## 10. CLI 映射

Native CLI 提供与 repository 无关的命令：

```text
graft delta create --base BASE --target TARGET --output DELTA [--json]
graft delta apply --base BASE --delta DELTA --output TARGET [--json]
graft delta inspect DELTA [--json]
```

这些命令不能要求或修改 `.graft` repository，并且必须拒绝全局 `--db`。选择
`--json` 后 stdout 只能包含 JSON，且要标识 operation、路径与格式元数据。

## 11. SDK capture 映射

SDK publication capture 可以直接从 immutable Graft snapshot 生成 `GRAFTD01`。
此时 opaque base token 必须保留下次 delta 所需的精确 base digest，结果必须暴露
完整 target SHA-256。SDK delta generation 对 repository history、ref、index 和
worktree identity 仍然是只读的。

## 12. 资源边界与安全

实现应逐 page 处理 SQLite image。网络 adapter 可以设置更严格的 delta 大小上限，
并只在该上限内缓冲。输入必须视为不可信：所有算术都要检查，分配或 seek 前必须
校验 page number，adapter 还必须把内嵌摘要与其使用的外部 immutable-object
identity 对齐。

## 13. 兼容性

104 字节 layout 是完整的 version 1 baseline。未来兼容扩展必须声明新的 header
size 和双方理解的 flags；只支持 version 1 的 reader 会拒绝它。语义或 entry 编码
变化必须使用新的 magic/version，不能做含糊猜测。
