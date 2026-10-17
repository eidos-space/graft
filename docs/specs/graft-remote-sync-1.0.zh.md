# Graft Remote Sync 1.0（中文参考）

状态：与当前实现对齐的规范草案
版本：1.0
发布日期：2026-08-11
规范语言：英文

## 摘要

本规格定义 remote config、backend 语义、HTTP protocol v1、immutable publication、
ref CAS、refspec，以及 `fetch/push/pull/clone`，使 filesystem、S3-compatible、HTTP、
native SDK 与 browser-capable 实现共享同一安全语义。

[Graft E2EE Remote 1.0](./graft-e2ee-remote-1.0.md) 定义带不透明路由提示的独立
加密 profile，提供可选原生源码实现。编码与批量端点仅在明确协商 profile 后适用，不改变
本文当前 plain 协议，也不代表已支持 E2EE。

## 1. 分层

Remote 传输三类状态：

```text
repository objects/packs          immutable
SQLite commits/segments + files  immutable
HEAD and refs/**                  mutable transactional metadata
```

Repository identity、snapshot content 与 pull 的 local integration 分别由 Repository、
Storage、Merge 规格定义。

## 2. Remote config 与 URI

| URI | 含义 |
| --- | --- |
| `memory` | process-local test backend |
| `fs:///absolute/path` | filesystem/mounted backend |
| `s3://bucket/prefix` | S3-style backend |
| `s3_compatible://bucket/prefix?endpoint=...` | custom S3-compatible |
| `https://host/<repository-path>` | canonical HTTP remote |
| `graft+https://...` | 显式 HTTPS alias |
| `graft+http://...` | 本地/可信 HTTP development |

Filesystem root 必须 absolute。S3 只存 bucket/prefix/endpoint，不能存 secret。
HTTP repository path 是 opaque base；client 移除 `graft+` 与一个 trailing slash。
CLI 的 `token_env` 可选择本地 env，但发请求前移除。1.0 拒绝 userinfo、fragment 与
其他 query。SDK 使用显式内存 credential，并拒绝 URL credential selector。

Remote config 是 local-only；除非 adapter 明确提供 validation，不会自动联网。

可选的 [E2EE Remote 1.0](./graft-e2ee-remote-1.0.md) profile 保存
`type = "encrypted_http"`、不含凭据的 `url` 和规范 UUIDv4 `uuid`。此 variant
固定为 `encrypted-remote-v1` 与 Suite 1，不能构造明文后端。缺原生模块或显式
注册会话时，必须在数据请求前失败。原生会话注册绑定远端名、规范化后的完整
地址与 UUID。秘密和私有 staging 路径不能进入仓库配置。账户/根密钥会话注册
是可信宿主的显式操作，不证明接入、安全存储或恢复流程符合 E2EE Remote 1.0。

会话注册期间，普通 bearer 替换/清除必须拒绝未经验证的账户变更。可信原生宿主
验证认证域与账户主体不变，并精确核对两个会话绑定后，可以刷新 token。此刷新
不能要求重新包装或锁定根密钥。换账户必须先排空任务并断开旧会话，再替换凭据。
仅 bearer token 的内容不能作为已认证身份的证据。

## 3. Credential 与 transport

HTTP 使用 `Authorization: Bearer <token>`。CLI 可从 `GRAFT_REMOTE_TOKEN` 或
`token_env` 读取；Rust/Node SDK 使用 session/call 显式内存 map。Credential 不能
进入 config、URL、cache、log、error、request ID 或 result；native secret buffer
应及时 zeroize。Production 应使用 HTTPS。当前 native client 为 read/probe/mutation
使用独立 HTTP/1.1 pool，connect timeout 5 秒、protocol request timeout 30 秒；S3
backend 另有 storage-layer retry。

## 4. Protocol 与 descriptor

所有 request/response 带 `Graft-Protocol: 1`；不支持应返回 `426`。`GET {base}`
返回 `graft-remote` descriptor、version、capability 与 limit。Capability 可包括：

```text
range list list-cursor put-if-absent read-bundle fetch-bundle upload-bundle
receive-pack receive-bundle multipart-object cas cad
```

Client 忽略 unknown field，并在 optional aggregate route 缺失时走 fallback。

## 5. Object key 与 backend

Known key：`HEAD`、`refs/heads/*`、`objects/*`、`objects/pack/*`、`store/files/*`、
`logs/*/commits/*`、`segments/*`。`HEAD`/`refs/**` transactional，其余 exposed v1
data immutable。`locks/**` 保留且不能暴露。

Path segment 独立 percent encode；空、`.`、`..`、backslash、slash-as-data、control、
invalid encoding 与 lock namespace 必须拒绝。当前 library object path 最大 768
UTF-8 bytes，comparison metadata 最大 16 KiB。

`objects/pack/<pack-id>.idx` 的 version 1 document 可以在 authoritative object
offset entries 之外附加 `commits` 数组，记录该 pack 内的 commit object ID 及 parent
object IDs。Client 只能把它作为选择 immutable pack 的 speculative prefetch hint；实际
使用 object 前仍必须 decode 并验证 content-addressed ID。Ancestry 缺失、不完整或错误时
必须 fallback 到普通的 verified object discovery，不能改变 fetch 结果。Writer 应为本次
publication 新增的每个 commit 写入 hint；reader 必须接受不含该字段的 version 1 index。

Backend 提供 byte-preserving head/get、transactional put/delete、put-if-absent、CAS、
CAD、sorted recursive list 与可选 multipart；必须 repository isolation。

## 6. V1 操作

| request | purpose |
| --- | --- |
| `HEAD/GET /raw/<key>` | existence/full/range read |
| `PUT/DELETE /raw/<key>` | transactional metadata |
| `PUT /raw-if-not-exists/<key>` | immutable create |
| `GET /list?prefix=...` | sorted recursive list |
| `POST /read-bundle` | 显式 immutable object 批量读取 |
| `POST /fetch-bundle/<ref-key>` | fetch 所需的 bounded reachable pack stream |
| `POST /cas`, `POST /cad` | atomic ref replace/delete |
| `POST /upload-bundle` | stable clone stream |
| `POST /receive-pack` | pack/index then ref CAS |
| `POST /receive-bundle` | dependencies + pack/index + ref CAS |
| multipart routes | resumable immutable transfer |

`HEAD /raw` 必需。只有明确 `405/501` 才可 fallback `GET Range: bytes=0-0`，auth/
transport error 不能触发。Range 遵循标准单 range，invalid/multiple 返回 `416`。

List recursive、bytewise lexical sorted、cursor-paginated；cursor opaque 且绑定 prefix。
Stable traversal 每个 matching key 恰好一次。当前默认 100、最大 500 per page。

## 7. Atomic create 与 compare

`raw-if-not-exists` 原子 create 或报告 existing；immutable 不能 unconditional overwrite。
CAS/CAD 用 expected-present 与 expected-hex，absence 与 present empty bytes 不同。
比较和 mutation 对单 key linearizable；mismatch 返回 `409` 且无修改。Branch ref
publication 必须 CAS/CAD。

## 8. Aggregate 与 multipart

Read-bundle 把有限个 immutable object read 合并为一次 authenticated request。Request
是 UTF-8 JSON：`{"version":1,"paths":[...]}`，包含 1 到 256 个 unique、valid
immutable path。Service 按 bytewise 顺序返回与 upload-bundle 相同的 `(path length,
object length, path, bytes)` network-byte-order frame。Response 使用
`application/vnd.graft.read-bundle`，以 `x-graft-bundle-objects` 声明精确 frame
数量，并通过 `x-graft-bundle-total-bytes` 与 `Content-Length` 声明完整精确长度。
任一 object missing 会使 aggregate request 失败；当前完整 response 上限 64 MiB。
Client 使用前必须验证 expected path、unique、length、final framing 与 object content；
`404/405/413` 时 fallback 到 bounded individual read。

Fetch-bundle 把 ref discovery、pack-index discovery 与 reachable pack read 合并为一次
authenticated request。Request 是 UTF-8 JSON：`{"version":1,"have":"<64-byte
lowercase object ID or null>"}`。Service 只把 pack index 的 `commits` ancestry 当作
bounded selection hint，以 upload-bundle 相同的 manifest/frame 格式 stream 选中的
`.idx` 与 `.pack`，并在返回前确认 ref 没有变化。Response 使用
`application/vnd.graft.fetch-bundle`；当前最多 128 packs、48 MiB。Client 必须 decode
并 content-hash 每个 imported object，之后仍完成普通 verified graph discovery，不能把
ancestry hint 当作 repository truth。`404/405/409/413/422` 时 fallback 普通 fetch；
auth、transport、malformed framing 与 object-integrity error 不能静默 fallback。

Upload-bundle 在 enumerate 前后两次读 ref；变化则 `409`。Stable response 是 manifest
加按 network byte order 编码、严格排序、唯一的 binary frame。V1 因 service opaque
会 bundle 全部 immutable key；client 在 temp local remote 验证后再 resolve/checkout。
当前最多 65,536 object；`404/405` fallback raw/list。

Response 使用 `application/vnd.graft.upload-bundle` 与
`x-graft-bundle-manifest-bytes`。Manifest 记录 version、reference path/value_hex、
object count；每 frame 是 4-byte path length、8-byte object length、UTF-8 path、body，
最后一个 frame 后必须立即结束。Response 必须通过
`x-graft-bundle-total-bytes` 声明完整 framed body 的精确字节数；host 允许为 stream
指定长度时，`Content-Length` 应为同一值。Client 必须优先使用 Graft total header
显示传输总大小，可为旧 service fallback 到 `Content-Length`；两者同时存在但不一致时
必须拒绝 response。

Receive-pack 先 immutable 创建 pack/index，最后 ref CAS；malformed/truncated body
不能更新 ref。Header 包含 64 位 lowercase pack ID、pack/index byte length、replacement
ref bytes 与 expected-present/expected-hex；body 精确为 `pack || index`。

Receive-bundle 增加 manifest byte length，manifest 中包含 1 到当前最多 256 个 unique immutable
snapshot/payload object，并逐项给出 path、bytes、`allow_existing`；body 精确为
`manifest || objects || pack || index`，最后 CAS。Length、path uniqueness 与 trailing
bytes 必须验证；`allow_existing=false` collision 返回 `412` 供 client verify/fallback。

Multipart 只改变 transfer，不改变 identity。Start/resume 绑定 upload ID/key/length/
part size；part 从 1 开始且可 retry replace；complete 在全部 exact part 存在后原子
暴露 object；abort 清 incomplete session。Complete 不发布 ref。
Routes 分别使用 `x-graft-object-bytes`、`x-graft-upload-id` 与
`x-graft-part-number`。非 final part 必须为 advertised size，final 为 remainder；同
key/length 重复 start 会 resume。当前 native client 最多 10,000 part，每个 missing
part 最多尝试三次。

## 9. Refspec

Refspec 是可选 `+` 加 `source:destination`，exact 或双方各恰好一个 `*`。Fetch
destination 必须在 `refs/remotes/<remote>/...`，push destination 在 `refs/heads/...`。
Push delete 为空 source 加 exact destination。Invalid/escaping/mismatched wildcard
必须失败。`+` 只允许该 mapping non-FF，不能禁用 expected-value CAS。

## 10. Fetch、push、pull、clone

Fetch 下载所选 remote ref 的 commit graph/object，expected-state 更新 remote-tracking
ref；不能移动 local branch、改 index、merge 或 materialize。它不必 hydrate 全部
SQLite segment/payload，因此 metadata 可用后细节读取仍可能 fetch。

Push 拒绝非 FF（除非显式 force）；相同 head 不重新发布不可变依赖或分支 ref。

对于仓库配置的默认分支，即使远端已是同一提交，push 也必须完成远端 symbolic
HEAD 的 create-if-absent，以恢复分支已发布而后续 HEAD 创建失败的情况。已有
HEAD 不得覆盖；不重新发布不可变依赖，也不更新已有同一提交的分支 ref。非默认
分支不初始化 HEAD。必需的 HEAD 步骤未确认时，此次 push 仍未完成，不推进其
remote-tracking ref，保留原跟踪状态。

数据发布顺序：

1. SQLite commit/segment 与 external payload；
2. repository object 或 pack/index；
3. 对 observed exact value CAS destination ref。

CAS 失败可留下 unreachable immutable data，但不能暴露 incomplete commit。Branch
delete 用 CAD。

原生 observed-head API 同时携带读取时的精确 ref、远端实例、分支、本地仓库和
远端配置绑定。绑定不同必须在发布依赖对象前拒绝。克隆远端保留观察身份，重新
构造远端不转移句柄。观察值不可序列化，也不能从目标提交重建。原生观察式 ref
读取限制为 16 KiB。这种绑定不证明读取后的最新状态；最终发布仍需精确 expected
CAS/CAD。加密观察值还遵守 E2EE 规范的会话与密文字节规则。

Pull 就是 fetch + 当前 branch 的 merge plan/apply，必须继承 up-to-date/FF/three-way/
conflict/stale/materialization 语义，不能另写 overwrite 算法。

Clone init 新 repository、配置 remote、通过 upload-bundle 或 fallback 得到 stable ref、
校验 immutable data、创建 tracking/upstream，再按 checkout/materialization 投影。
已有 non-empty destination 不能静默覆盖。

## 11. Status、retry 与 publication uncertainty

关键状态：`409` 是 ref expected mismatch，`412` 是 create-only collision；此外
`400` malformed、`401/403` auth、`404/405` missing/optional fallback、`413/414`
limit、`416` range、`423` lock、`426` version、`429` rate、`500/503` service failure。

Read 与 idempotent immutable create 可 bounded retry。CAS mismatch 不是 transport
retry。若 ref publication request 发出后连接失败，adapter 必须区分 known reject、
publication unconfirmed 与 outcome unknown；在发布不同 update 前先 inspect/reconcile。
Cancellation 也只能保证 safe boundary。

## 12. Conformance

Service 必须 durable read-after-write、byte preservation、atomic create-only、per-key
linearizable CAS/CAD、repository isolation，以及无 concurrent mutation 时 complete
stable-prefix list。

至少测试 URI/credential、protocol/key validation、range/head/list、collision/CAS/CAD、
malformed aggregate 阻止 ref、capability fallback、refspec、fetch isolation、push
ordering/concurrency、pull=fetch+merge、clone safety 和 publication uncertainty。

当前证据位于 `crates/graft/src/repo/sync.rs`、remote/runtime action、
`packages/graft-remote` 及其 Hono/Cloudflare test、CLI/SDK integration test。

## 13. 兼容说明

- Legacy `/api/graft/v1/repos/...` 可作 alias，client 不自行插入。
- Optional aggregate operation 必须保留 raw/list/CAS fallback。
- V1 upload bundle 列举全部 immutable key；reachability negotiation 留待后续。
- HTTP/fs/S3 内部 transaction 可不同，但 publication 语义必须相同。
