# Graft Remote Sync 1.0（中文参考）

状态：与当前实现对齐的规范草案
版本：1.0
发布日期：2026-08-11
规范语言：英文

## 摘要

本规格定义 remote config、backend 语义、HTTP protocol v1、immutable publication、
ref CAS、refspec，以及 `fetch/push/pull/clone`，使 filesystem、S3-compatible、HTTP、
native SDK 与 browser-capable 实现共享同一安全语义。

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
range list list-cursor put-if-absent read-bundle upload-bundle
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

Push 拒绝非 FF（除非显式 force）；相同 head 不做多余 publication。顺序：

1. SQLite commit/segment 与 external payload；
2. repository object 或 pack/index；
3. 对 observed exact value CAS destination ref。

CAS 失败可留下 unreachable immutable data，但不能暴露 incomplete commit。Branch
delete 用 CAD。

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
