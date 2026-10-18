# Graft E2EE Remote 1.0（中文参考）

状态：定稿设计规范；Unix 和 Windows 原生宿主提供可选源码实现；生产验证尚未完成
版本：1.0
设计日期：2026-10-12
规范语言：英文
Profile：`encrypted-remote-v1`

## 摘要

本规格定义 Graft HTTP 远端的客户端加密，以及服务端可读取的不透明同步提示。
内容、路径、分支名、提交说明和权威索引加密。服务端可见不透明提交的父子关系、
提交与 pack 的归属，以及 ref 路由目标，以选择加密 pack 并保留批量同步能力。

客户端随机生成仓库根密钥，用独立同步密码派生的密钥包装。可信宿主可以在系统
安全存储中记住根密钥。已授权设备可以修改密码，无需重新加密历史。

要求是实现目标，不是当前支持证据。生产声明需要第 20 节的证据与独立安全评审。
本文与[英文规范](./graft-e2ee-remote-1.0.md)按章节对齐，仅作信息性参考；英文为准。
英文全大写 MUST、MUST NOT、SHOULD、SHOULD NOT、MAY 按 BCP 14 解释。

## 1. 范围与职责

本规格拥有加密 profile、密码学编码、公开路由元数据、密钥生命周期和加密 HTTP
扩展。它特化[Remote Sync 1.0](./graft-remote-sync-1.0.md)，不改变 plain 远端。
[Repository](./graft-repository-1.0.md)、[Storage](./graft-storage-snapshots-1.0.md)、
[Merge](./graft-merge-1.0.md)、[Worktree Materialization](./graft-worktree-materialization-1.0.md)
继续拥有对象身份、快照、集成和物理文件语义。

v1 面向个人多设备的 Rust、CLI、Rust/Node SDK 及 Lite 等可信原生宿主，使用
HTTP。Fs/S3、浏览器同步、团队密钥分发、设备配对、恢复码槽、作者签名和原地
根密钥轮换不在此版本范围。

创建远端时选择加密。一个远端固定 profile 和 UUID，不能混合明文与加密命名
空间。本地 SQLite 与普通文件保持明文。锁定同步不能描述成加密或锁住本地文件。
明文转加密需要新远端及第 18 节迁移。

## 2. 威胁模型与披露

服务端可能读取、修改、替换、回放、遗漏、分叉或删除数据。客户端、密码学随机
源和已解锁设备被视为可信。

| 信息 | 处理 |
| --- | --- |
| 文件/SQLite 内容、逻辑路径、分支名、提交说明、完整 pack 索引与偏移 | 客户端加密 |
| 密码、根密钥、派生密钥 | 不发送给服务端 |
| 账户、随机 UUID、profile/suite、对象类别、大小/数量、时间和重复访问 | 可见 |
| 不透明提交的父子边、提交与 pack 归属 | 明确公开 |
| 不透明 ref 目标、分支推进、symbolic HEAD 目标、附注标签 ref 类型及路由提交、合并拓扑 | 明确公开 |
| 仓库内相等关系、批量/请求组合、孤儿对象 | 可观察 |
| 明文内容哈希与 ID | 禁止公开 |

不透明标识阻止没有仓库密钥的一方直接计算，但不隐藏拓扑或全部活动。诱导上传
指定内容结合观察，仍可能确认猜测。不做跨仓库去重。

AEAD 和已认证提示检测修改与身份替换。本机配置检查点检测已观察配置的回退。
v1 不提供完整 ref 防回滚、全局最新性、分叉检测、列表完整性、可用性或历史副本
安全擦除。首次接入信任成功认证的配置，不能证明它是最新版本。

所有根密钥持有者共享同一密码学写入权限，对称 MAC 不识别具体作者。本地明文、
已解锁设备被攻破、获授权插件、恶意客户端更新和内存转储不在保密保证内。账户
访问控制与密钥访问分开。

## 3. 分层与必需覆盖

```text
Repository / Storage / Sync / Merge
             ↓ 逻辑键、明文、观察值
       PlainRemote | EncryptedRemote
             ↓ 不透明键、密文、路由提示
             OpaqueTransport
             ↓
        HTTP 服务端 / 存储

可信宿主 KeyProvider → 原生 SecretSession → EncryptedRemote
账户 CredentialProvider → OpaqueTransport
```

EncryptedRemote 必须覆盖 raw/commit/segment 读写、range、hydration、发现、clone、
批量接口和 multipart。优化及后台存储操作不得绕过。缺密钥、模块、配置或支持时
停止，不回退为传输明文。

Graft 拥有加密和已认证规划契约；宿主拥有输入、系统存储、新鲜本机认证、UI 与
迁移协调；服务端拥有账户授权、配额、字节传输和原子 CAS/CAD，不要求明文索引
或密钥。

Transport、credential 和可信 key provider 可以是编译期扩展。不允许任意 JS
加密回调、动态算法插件或普通插件访问 KeyProvider。

## 4. 算法组合与规范编码

Suite 1 固定为 XChaCha20-Poly1305（16 字节 tag、24 字节 nonce）、HKDF-SHA-256、
HMAC-SHA-256、SHA-256 和 Argon2id v1.3。随机值使用系统 CSPRNG。未知
suite/format/KDF ID 必须拒绝。

U8/U16/U32/U64 为 unsigned big-endian。长度不含自身前缀。E(parts...) 为 U16
字段数，随后每个字段是 U32 字节长度与原字节。整数字段包含精确固定宽度编码。
嵌套列表用 E 编码每个 entry；空列表是零字段 E。`||` 是字节拼接。UUID 为
16 个原字节，ID 在二进制结构里为 32 字节，在 URL/JSON 中为 64 位小写 hex。
文本是合法 UTF-8，不 normalization 或大小写折叠。

解析器必须要求精确字段数、顺序、宽度、枚举与输入结束，按唯一性规则拒绝
重复项，分配前检查长度，使用 checked 算术。逻辑键遵守 Graft 现有规则且不超过
4096 UTF-8 字节。资源上限见第 16 节。每项密码学操作使用下文指定的域标签。

## 5. 密钥体系与密码

创建时生成 UUIDv4 和随机 32 字节 K_root。每项用途派生：
`HKDF-SHA-256(IKM=K_root, salt=UUID,
info=E("graft/e2ee/v1", profile, U16(1), purpose), L=32)`。
profile 是 UTF-8 字符串 `encrypted-remote-v1`。

| purpose | 用途 |
| --- | --- |
| name | 逻辑对象名字 HMAC |
| header | 每次上传的头密钥派生 |
| payload | 每次上传的数据密钥派生 |
| ref | 可变 ref/HEAD AEAD |
| config | 配置 MAC |
| route | 不透明提交路由 ID |
| hint | 公开提示 MAC |

KDF profile 1：Argon2id version 0x13、m=65536 KiB、t=3、p=4、32 字节输出、
32 字节随机 salt，无 Argon2 secret/associated-data 输入。不得静默降低参数。
线程数可以较少，但 lanes 和结果不变。KDF 并发有界。

密码是 1–1024 字节合法原始 UTF-8，不 trim 或 normalization。JS 拒绝未配对
surrogate。宿主应建议密码管理器生成强密码，允许粘贴，精确保留输入。wire 接受
短密码不代表建议短密码。攻击者可以针对包装密钥离线猜密码。

K_wrap = Argon2id(password, salt, profile_1)。默认不得持久保存密码或 K_wrap，
不再使用时应尽力清零。

## 6. 配置与接入

`e2ee/config` 为固定 206 字节可变对象，字段按表顺序：

| 字段 | 宽度 |
| --- | --- |
| ASCII magic `GRAFTEC1` | 8 |
| format=1、suite=1 | 各 U16 |
| UUID | 16 |
| generation，初始为 1 | U64 |
| mutation_id | 32 个随机字节 |
| kdf_profile=1 | U16 |
| salt | 32 个随机字节 |
| wrap_nonce | 24 个随机字节 |
| wrapped_root，含 tag | 48 |
| config_mac | 32 |

C0 是截至 salt 的原始字段拼接，C1 是截至 wrapped_root 的原始字段拼接。
用 K_wrap 包装 K_root，AAD 为 `E("graft/config-wrap/v1", profile, C0)`。
`config_mac = HMAC-SHA-256(K_config,
E("graft/config-mac/v1", profile, C1))`。

KDF 前验证长度、ID 和已固定身份；解包后验证完整 MAC。错误密码或解包/tag
失败统一 UnlockFailed，不区分密码错误与包装损坏。已有根密钥的会话无需旧密码
即可验证配置 MAC。

接入后固定 UUID/profile/suite，在宿主私有状态崩溃安全保存检查点
`(UUID, generation, SHA256(config_bytes))`。较低 generation 或同 generation
不同摘要必须失败。较高且已认证版本可推进检查点，但不证明唯一后继或全局最新。
generation 溢出失败。检查点丢失需要显式重建本机信任。

检查点验证和持久推进成功后，才能放行使用该配置的联网数据操作。存储失败返回
CheckpointUnavailable，不更新内存信任状态，也不缓存已验证会话 epoch。
即使异步调用方退出，持钥的保存任务仍属于会话锁定与排空边界。

首次接入依靠成功解包与 MAC 建立信任，不依靠服务端声明。已接入远端丢失配置
必须返回 RemoteConfigMissing，不重新初始化或降级。

## 7. 逻辑命名与加密对象

规范化逻辑键 L 和类别 C：

```text
T = HMAC-SHA-256(K_name, E("graft/name/v1", profile, C, L))
remote_key = "e2ee/" + C + "/" + lowercase_hex(T)
```

类别为 objects、packs、indexes、files、commits、segments、refs、head。分类保持
现有键身份，不合并内容相近的不同键。原逻辑 `objects/pack/*.pack` 对应 packs，
`objects/pack/*.idx` 对应 indexes，其他 `objects/**` 对应 objects，
`store/files/**` 对应 files，`logs/<log>/commits/<lsn>` 对应 commits，
`segments/**` 对应 segments，`refs/**` 对应 refs，HEAD 对应 head。未知类别拒绝。

不可变密文公开前缀共 72 字节：ASCII GRAFTEO1、U16 format=1、U16 suite=1、
32 字节随机 upload_id、24 字节随机 header_nonce、U32 encrypted_header_length。
加密头含 tag 不超过 16 KiB。

每次上传的密钥通过 HKDF-SHA-256 派生，IKM 分别为 K_header/K_payload，salt
为 upload_id，info 分别为
`E("graft/upload-header/v1", profile, UUID, remote_key)` 和
`E("graft/upload-payload/v1", profile, UUID, remote_key)`，L=32。

头明文为 E(logical_key, class, U64 plaintext_length,
32-byte plaintext_sha256, U32 chunk_size, U64 chunk_count, U32 flags=0)。
头 AAD 为 `E("graft/object-header/v1", profile, UUID, class, remote_key,
public_prefix)`。

数据块可为 16/64/256 KiB，默认 64 KiB。第 i 块 nonce 为 16 个零字节 || U64(i)。
AAD 为 `E("graft/object-chunk/v1", profile, UUID, remote_key, upload_id,
SHA256(encrypted_header), U64(i))`。新加密尝试生成新 upload_id 和头 nonce；
不同明文不得复用 upload_id。

块数为 max(1, ceil(plaintext_length/chunk_size))。空对象含一个认证空块，非末块
明文恰为 chunk_size。客户端验证键映射、类别、flags、长度、块数和总密文长。
完整读取检查明文 SHA-256 和 Graft 权威身份。不能假定 SegmentId 是明文 hash。
不可变字节不覆盖。

## 8. 范围读取与本机缓存

读取 [a,b) 时先认证有界头，再获取覆盖范围的完整加密块，逐块认证后裁切。
B 为 chunk_size，H 为头结束偏移，第 i 块从 `H+i*(B+16)` 开始，末块长度由
已认证 plaintext_length 得出。空范围不读 payload；越界或溢出失败。

部分读取认证头与覆盖块，不证明完整文件摘要。头缓存绑定 UUID/key/upload_id，
块缓存额外绑定序号及认证状态。加密层不引入整文件压缩，上层布局不变。

64 KiB 块中的 4 KiB 冷读约有 16 倍字节放大，还不含头开销；性能声明必须测量。
发送方可以选择其他规定块长，不改变明文身份。

本机解密缓存按本地明文模型保存并限制文件权限。锁定同步不擦除已交付明文，
也不加密临时 SQLite 数据库。服务端及上传 spool 应保存密文。
上层不可变快照可以在加密前使用临时明文 spool，但必须限制大小、在首次写入前
具有仅所有者可访问的权限，并在最后一个句柄关闭时删除。它不能成为持久恢复
状态；保留的上传候选仍必须是密文。

会话内发现缓存可以保存有界的密文头，用于列表中的不可变对象逻辑身份发现。
缓存必须绑定已固定的账户、仓库、profile、suite、端点和已准入的秘密会话代次，
解释元信息前必须重新认证缓存字节。锁定、断开或替换秘密会话必须使此缓存失效，
旧操作不能重新填充。缓存的发现身份不能替代新鲜远端存在性检查、ref 观察值、
碰撞检查或载荷认证。列表身份不能证明对象当前可用。

## 9. 已认证不透明路由提示

### 9.1 提交 ID 与提示身份

对权威提交 ID 的 32 个原字节 c：
`route(c) = HMAC-SHA-256(K_route,
E("graft/route-commit/v1", profile, c))`。
路由 token 不替换加密内容中的 Graft 权威 ID。

pack P 的 hint key 是 `e2ee/hints/<pack-token>`，pack-token 来自 P 的
`e2ee/packs/` 不透明键后缀。提示明文不可变，由客户端认证，明确供服务端读取。

前缀为 ASCII GRAFTEH1 || U16 format=1 || U16 suite=1 || UUID。
body 为 E(opaque_pack_key, opaque_index_key, E(commit_entries))。
每项为 E(route_commit_id, E(parent_route_ids))。entry 按原始 token 排序且唯一；
parent 保持权威顺序且唯一。每个 parent 为 32 字节，每项最多 32 个 parent。
提示最多 4096 个 entry，含 MAC 总长不超过 1 MiB。

提示字节为 prefix || body ||
`HMAC-SHA-256(K_hint, E("graft/routing-hint/v1", profile, UUID, hint_key,
prefix, body))`。读取方验证 MAC、身份、上限、排序、键类别及 key/pack-token
关联。复用 hint key 要求完全相同的字节。

发布方对含提交且能满足上限的每个 pack 必须生成完整提示，应拆分 pack 以保持
可表示。不含提交的 pack 无需提示。超出表示能力的历史拓扑可以省略提示并走
已验证回退，但不得截断 parent 或改变仓库语义。

### 9.2 权威性与服务端索引

提示只辅助预取/发现，不是仓库事实。客户端必须对照解密 pack/index，依据真实
提交及 parent 重新计算 token，并验证权威对象 hash。每个提示提交必须存在于
对应 pack。即使服务端宣称已返回全部数据，也必须完成已验证依赖发现。

服务端不能验证 MAC，按不可信输入有界解析。派生图索引可保存 token → 有序
parent 与多个 pack/index/hint tuple。同一提交出现于多个 pack 合法；parent
冲突时禁用该规划路径，不静默选一份。服务端应增量索引新提示，允许从提示重建，
不要求解密索引。

派生索引可丢弃。提示缺失、过时、不完整、缺图节点、不支持拓扑或所指数据缺失
时需要 hint-unavailable 回退。客户端不把缺提示解释成缺历史。

## 10. 带路由的 ref 与精确观察值

ref/HEAD 的公开前缀是 ASCII GRAFTER1 || U16 format=1 || U16 suite=1 ||
U8 route_kind || 32-byte route_target || 24-byte random nonce，共 69 字节。
kind 1 为直接提交，target 是 route(target_commit)。kind 2 为 symbolic HEAD，
target 是目标不透明 ref key 后缀的 32 个原字节。只有 HEAD 可用 kind 2。
不存在的 ref 没有 envelope。
kind 3 为附注标签 ref，target 是 route(tag_commit)。只有逻辑
`refs/tags/...` 可以使用 kind 3。权威标签对象 ID 加密，不能替换成提交 ID。

加密体为 E(logical_ref, authoritative_target, 32-byte random mutation_id)。
symbolic HEAD 目标使用 `ref: refs/heads/<normalized_branch>`，不含末尾换行；
直接提交目标也不含末尾换行。
文本遵守已有 ref/target 规范，kind 1 的 target 是小写 Graft commit ID 文本。
用 K_ref，AAD 为 `E("graft/ref/v1", profile, UUID, remote_key, public_prefix)`。
整个 envelope 不超过 16 KiB。

kind 3 的 authoritative_target 精确为
`tag: <tag_object_id> commit: <commit_id>`，两个 ID 都是 64 位小写 hex，
指定字段间仅一个空格，不含末尾换行。客户端必须认证 envelope，重新计算
路由提交，并验证实际编码标签对象的身份、类型及声明的提交与两个加密 ID
一致，之后才能信任这条依赖边。标签字节和完整提交依赖必须在最终 ref CAS 前
发布。轻量标签使用 kind 1。不透明服务端只在 `e2ee/refs/` 接受 kind 3，
不能用于 `e2ee/head/`；服务端不能验证加密的逻辑标签名或内容。
不支持标签传输时必须失败，不能解引用标签或换用其他路由类型。只收到一个 ID
的通用 ref 写入接口不能猜测它代表提交还是附注标签，必须使用有类型且已验证
的元数据。

客户端认证后依据解密值重新计算 public kind/target，绑定路由与精确 ref。
不存在单独可变的 ref 路由 sidecar；分支名保持加密。

RefObservation 绑定 UUID、key、session generation、实际 envelope 字节或 absent。
CAS/CAD 比较精确字节，不能通过重新加密 expected 明文构造。根密钥/会话身份
不变时改密码不使观察值失效；锁定或重连使句柄失效。

删除用精确 CAD，重建用 absent CAS。HEAD 首次 create-if-absent，随后精确 CAS。
多 ref push 保持逐项原子并报告部分成功，不引入全仓库 ref 锁。

## 11. HTTP profile 与批量 framing

请求/响应保留 Graft-Protocol: 1。GET base 声明 profile、encryption version=1、
suite=1、UUID、capabilities 和 limits。已认证 config 才是密码学身份来源；
descriptor 不一致失败。生产必须 TLS，仅隔离 loopback 测试可明文 HTTP。
bearer 凭据不跟随跨域重定向。

Descriptor 保留顶层 protocol、version、capabilities、limits，增加下列精确
encryption 对象。UUID 是与 config 的 16 字节对应的小写带连字符 UUIDv4 文本；
encryption.version 独立于 HTTP protocol version。

```json
{
  "protocol": "graft-remote",
  "version": 1,
  "encryption": {
    "profile": "encrypted-remote-v1",
    "version": 1,
    "suite": 1,
    "uuid": "123e4567-e89b-42d3-a456-426614174000"
  },
  "capabilities": [
    "range", "list", "list-cursor", "put-if-absent", "cas", "cad",
    "e2ee-read-bundle", "e2ee-fetch-bundle", "e2ee-receive-bundle",
    "e2ee-multipart-object"
  ],
  "limits": { "max_request_bytes": 67108864, "multipart_part_bytes": 8388608 }
}
```

两个字节上限只是示例，不是必需默认值。沿用 descriptor 未知字段处理，客户端
验证全部必需 encryption 字段，拒绝未知 encryption version/suite/profile。

加密服务端拒绝 plain key 和旧语义批量路径。合法键为 config、第 7 节不透明键、
第 9 节 hint key。客户端显式选择并固定 profile，旧 plain 客户端不能静默接入。

禁用无条件 raw PUT/DELETE。只有 config、refs、head 接受 CAS/CAD；不可变对象
及 hint 用 create-if-absent 或 receive/multipart completion 创建。可变项不得
作为 immutable batch frame。服务端 mutation 前检查 envelope 结构、声明身份
和键类别，不宣称验证 MAC。JSON 控制 body 拒绝重复/未知字段与错误类型。

base 必需能力为 range、分页 list、immutable create-if-absent、精确 CAS/CAD、
e2ee-read-bundle。优化能力增加 e2ee-fetch-bundle、e2ee-receive-bundle、
e2ee-multipart-object。

| 相对 base 的端点 | 契约 |
| --- | --- |
| 现有 head/raw/range/list/put-if-absent/cas/cad | 不透明键、精确字节操作；按规则更新 config/ref |
| POST /e2ee/read-bundle | 明确指定不可变不透明键，返回密文或公开已认证提示 |
| POST /e2ee/fetch-bundle/<opaque-ref-key> | 路由规划，返回有界 pack/index/hint 批量 |
| POST /e2ee/receive-bundle/<opaque-ref-key> | 不可变 dependency/pack/index/hint 批量，再精确 ref CAS |
| 现有 multipart 路径，声明 e2ee-multipart-object | 第 14 节 identity 与密文语义 |

opaque-ref-key 按既有规则逐路径段 URL 编码。fetch/receive 只接受 refs key，
symbolic HEAD 由客户端认证后解析。缺可选能力只允许加密 primitive 回退。

批量二进制为 GRAFTEB1 || U32 manifest_length || manifest || frames。
manifest 是 E(U16 version=1, reference_key_or_empty, reference_bytes_or_empty,
E(frame_descriptors))。每个 descriptor 是 E(opaque_key, U64 length)。frame
是 U32 key_length || U64 object_length || key UTF-8 || 精确对象字节。
descriptor/frame 的键唯一、按字节排序，顺序与长度相等，无尾随字节。frame
数量是嵌套列表字段数。manifest 最大 256 KiB，ref 最大 16 KiB，其余见第 16 节。

响应以 x-graft-bundle-objects 和 x-graft-bundle-total-bytes 声明精确数量与
framed 长度，可用时 Content-Length 应一致；始终检查实际字节。带 body 请求
要求精确 Content-Length。read/fetch 响应及 receive 请求的 Content-Type 是
application/vnd.graft.e2ee-bundle。

## 12. Fetch、pull、clone 与回退

Read JSON 精确为 `{ "version": 1, "keys": [opaque_key, ...] }`，包含 1–256
个唯一不可变键，可含 hint。响应 reference 字段为空。缺对象使整批失败，可以
有界逐项重试。不接受可变 config/ref。

Fetch JSON 精确为 `{ "version": 1, "have": token_or_null }`。have 是最后
本地验证的 fetched commit 的 64-hex route token，不能仅凭未验证 hint 广告。

服务端读取精确 ref，从 route_target 遍历到 have 或已知根，选择覆盖的
pack/index/hint，再读 ref。字节改变返回 409；提示覆盖/规划不可用或超限返回
422；缺图数据不能返回所谓空历史。必须检查环和限制遍历。have 等于 target 时
可返回零 frame 和精确 ref，但客户端仍检查解密后的真实 target。

响应包含加密 pack/index、完整公开 hint，manifest 包含观察的 ref。客户端
验证 framing、ref AEAD/路由、hint MAC、对象 envelope、明文身份和可达性。
确认完成前通过加密操作补齐被遗漏的权威依赖。
Manifest reference_key 必须等于请求的 ref key；返回 ref 缺失、不同或无法认证
时失败。

缺能力、404/405/413/422 或明确 409 竞争允许有界加密发现/重试。认证错误、坏
framing、hint MAC 失败、对象完整性失败不得静默降级为成功回退。已认证 hint
与已验证明文矛盾时报告 HintMismatch；显式已验证重试可忽略该提示，但成功结果
不能隐瞒问题。

Primitive 路径认证选定 ref，分页列举不透明 indexes，解密所需 metadata，利用
已验证缓存在客户端规划。不能信任列表完整性或读取无关明文工作树。冷发现可能
O(N)，普通已缓存/优化 fetch 应避免重扫全部索引。

Fetch 不移动本地分支、index 或 worktree；pull 仍为 fetch 加既有 merge。Clone
固定已验证 ref target，获取完整所需闭包。分支持续变化时批量路径可能不可用，
primitive 可从固定不可变 target 继续。克隆目标必须不存在或为空目录；已存在的
非空目标必须在替换文件前拒绝。使用显式注册远端名称的原生克隆，必须保持该名称
对应的动态凭据、固定加密身份，以及会话锁定／断开的生命周期。

## 13. Push 与加密 receive

客户端在发布前构造并本地验证密文与提示。按顺序发布 snapshot/payload 依赖、
pack 和完整加密 index、已认证 hint，最后精确 ref CAS。ref 不得指向未发布完整
依赖集合。

Receive manifest 包含 ref key 和候选 ref 字节，排序 frame 最多 256 个不可变
对象/hint。x-graft-expected-present、x-graft-expected-hex 按 Remote Sync
编码精确旧 ref/absent，replacement 仅放 manifest。服务端有界解析身份/长度，
接收完整 immutable frame，拒绝截断/尾随字节，全部可用后才最终 CAS。
服务端不能验证 MAC 或明文，客户端发送前完成检查。

已有相同密文/hint 可以复用。同 key 不同密文返回 412 且不最终 CAS，不覆盖。
相同明文的随机加密可能合法地产生不同字节。客户端认证已有对象头、逻辑键、
明文长度/摘要和总密文长后决定能否复用；这不证明远端完整可用。Hint 要求
精确字节相同且 MAC 有效，已认证冲突是错误。

良性碰撞后可以通过加密 primitive 发布剩余数据再精确 CAS。服务端不能凭随机
密文判定明文相等，不提供明文摘要 API。失败可能留下已上传对象；framing、
碰撞或不完整上传错误时 ref 不推进。

已验证 head 未变应避免多余发布，已验证缓存应减少探测。不同 ref 独立推进，
同 ref 竞争遵循快进/merge 规则，不无条件覆盖。

## 14. Multipart、重试与结果不确定

Remote Sync 1.0 的 CAS/CAD 预期字节不匹配使用 HTTP 409；仅创建操作的冲突使用
412。协调 ref 或 config 修改结果时，客户端必须识别相应的不匹配响应，并在
判断当前值之前认证读回内容。单独的响应状态不能确认候选，也不能认证竞争值。

Multipart start 额外绑定 UUID、opaque key、加密 upload_id、总密文长度和完整
密文 SHA-256。Hint 较小，不用 multipart。server session_id 与 encryption
upload_id 不同。恢复要求全部身份匹配、相同密文字节，禁用旧 key/length-only
策略。传输分片边界遵守 server limits，不等于明文加密块边界。

Start 请求在既有 key/总字节字段之外，携带 x-graft-e2ee-uuid（规范 UUID 文本）、
x-graft-e2ee-upload-id（64-hex 加密 upload_id）、
x-graft-e2ee-ciphertext-sha256（64-hex 摘要）。服务端记录并绑定 session_id，
在 start/resume 响应回显供客户端验证。Complete 验证实际组装密文长度与 SHA256
后才发布。该 hash 不证明明文身份。不同加密 upload_id 的 session 不能合并。

客户端保留密文 spool，或足以重建相同不可变快照、upload_id/nonce 的受保护记录。
complete 原子 create-if-absent，不暴露部分对象；竞争方验证胜出对象。abort/TTL
只清理未完成会话。

服务端必须将分片写入准入与完成操作串行化：替换写入仍在进行时，不能冻结此前
已确认的分片。写入结果未知时，必须跨重启保留待定状态，直到完成核实或显式中止。
不可变对象发布必须原子检查精确上传会话仍活跃且有效；中止或拒绝前已开始的
验证器不能在其后发布。中止不能删除已发布的胜出对象。显式中止可以退休已冻结
但尚未发布的会话，包括底层存储上传已过期的会话，然后以相同的保留密文身份
建立新服务端会话。客户端必须确认中止成功后才重启，并向新会话重传所有分片。

每次 ref/config 更新固定旧字节、新字节和 mutation_id。响应丢失后重读：等于
候选确认成功，等于旧值允许原样重试；其他值仅在有 mismatch 证据时确认冲突，
否则 OutcomeUnknown；无法读取也是 OutcomeUnknown。后继提交不能证明此请求
成功。确认后才更新 remote-tracking refs。

取消/锁定不能证明已发送 mutation 未生效。持久 journal 不含明文密码/密钥，
secret 只在获准 provider 中。不能重新加密重试候选。

恢复保留的 ref 候选是可信宿主的显式操作，不会复活已失效的会话句柄。
Journal 必须把记录绑定到应用、账户和仓库身份。当前获准会话必须认证保留的
旧 ref 和新 ref 密文，验证不透明键与逻辑 ref 的关联，再绑定新的观察句柄。
记录缺失或损坏时必须失败，不能静默生成替代候选。修改请求发送前必须确认
journal 绑定已持久化。即使调用方退出，持有密钥的持久化任务仍留在会话锁定
与排空边界内。
未提供持久 ref journal 的宿主注册可以读取加密数据，但不能发布可变引用。分支、标签、HEAD 和
aggregate receive 的修改都必须在发送前拒绝缺失的恢复存储；不能通过底层
发布回退绕过日志。
幂等清理必须核对保留的精确退役记录；空槽不能证明所请求候选已退役。原生检查点
校验和用于发现本地意外损坏，不能防御恶意本地管理员；记录丢失后不能重建既有信任。

结果未知或尚未确认时，不同操作不能替换该 ref 的活动恢复槽。清理恢复槽必须先
确认发布成功或取得明确冲突证据，精确比较保留候选，并确认清理已持久化。
延迟到达的清理不能删除较新的候选。不同 ref 使用独立恢复槽。

恢复命名空间必须区分显式首次登记与重开。新恢复槽获准使用前，必须持久保存
绑定身份的分配证据。已获准使用的槽的目录或记录缺失时必须停止，包括进程重启之后；
重开丢失或被重新创建的命名空间时不能静默登记。分配证据缺失或损坏时必须停止
修改请求准入。续建被中断的首次空分配，必须有该槽从未获准使用的持久证据。
把槽返回给任何调用方之前，准入状态必须已持久化。已有活动记录或损坏记录不能
被空初始化替换。没有准入状态的旧分配证据必须视为可能已获准使用。不能仅凭
缺失推断从未存在已获准的候选。命名空间分配锁不能在远端发布期间持有。

把加密凭据绑定到仓库之前，可信宿主必须验证检查点、恢复目录和临时 staging
与实际工作树及仓库元数据分离。验证必须解析路径别名和父级仓库发现；任一方向
的包含关系都属于重叠。验证必须先于运行时修改或远端请求。新目标的放置验证
不能要求先创建文件系统状态。改变目标或文件系统布局时，宿主必须维持这种分离。

## 15. 改密码、存储与会话生命周期

忘记密码后的重置授权必须由可信原生宿主提供，绑定精确的应用、认证域、账户和
仓库。必须进行新鲜系统认证；账户登录、读取已记住的密钥、缓存同意或普通插件／
JS 声明不能替代认证。授权器不能接收根密钥。认证及后续重新包装必须保持获准
会话的操作所有权，留在锁定／排空边界内，包括调用方取消；认证期间会话失效时
必须拒绝继续。准备重置候选不代表已经发布它，也不代表密码恢复已经完成。

修改密码需要解锁根密钥会话及新鲜宿主授权：本机系统认证或重输当前密码。
仅账户登录不够。忘记旧密码后的恢复要求可用根密钥与可信本机认证。

认证 config/检查点，生成新 salt、nonce、mutation_id，generation 加 1，包装
同一根密钥，本地验证，再精确 CAS。确认成功后更新检查点并提示完成。并发失败
方不自动覆盖。其他解锁设备继续使用，以后新设备输入新密码。

准备改密码候选时，必须根据刚观察到的当前 config 重新认证，不能使用缓存的
旧密码包装。仅准备候选不能发布它，也不能把检查点推进到该候选。修改请求发送
前，保留事务必须保存精确的旧／新 config 字节，并绑定到应用、账户和仓库身份。
恢复时必须用获准根密钥认证两份 config，要求 UUID 一致且 generation 恰好增加
一代，不能生成新的包装字节。密码 KDF 和本机验证任务必须由获准操作持有，
留在取消及锁定／排空边界内，即使调用方退出。

即使收到成功响应，config 修改结果也必须通过已认证读取来确认。仅精确的新
config 能确认本候选；精确的旧 config 允许重试。不同值仅在有 CAS mismatch
证据时确认冲突，否则结果仍未知。读取缺失、损坏、身份不符或回滚／分叉时，不能
确认成功或清理事务。成功确认要求检查点推进已持久化，并精确清理 journal；确认
持久化失败时仍为 OutcomeUnknown。不同待确认候选不能互相替换。不发送 CAS 的
检查可以在确认后推进本地恢复状态，但不能修改远端。

退休 config 事务前，必须根据最新的共享持久化检查点和会话检查点验证所观察的
config，并在退休完成前排除检查点的并发推进。有效的失败方冲突不能推进任一
检查点。过时会话缓存的检查点不足以完成此验证；无法完成该原子验证时，结果
仍为未知，并保留活动事务。

退休事务必须在身份绑定的本地持久状态中保留最后一次候选的精确字节，直到被
较新的事务取代，以便重开后恢复丢失的退休确认。退休记录不再是活动事务，也
不能作为成功证明：恢复仍须认证当前远端 config，并遵守上述确认规则。加载
活动或退休恢复记录必须是原子操作。延迟退休不能擦除较新记录。退休后绑定
不同候选时，其 expected generation 不能低于退休候选的 replacement generation。
缺少记录不能解释为结果已确认。旧格式的空记录不提供退休证据。

旧密码加旧包装记录仍可得到根密钥，改密码不是撤销设备。撤销 token 不擦除密钥。

记住的根密钥必须由系统 Keychain/Keystore 或系统保护的包装密钥保护，绑定应用、
认证域、账户主体、UUID/profile/suite。凭据文件位于仓库外，owner-only 且崩溃
安全替换，不进同步、插件存储、日志或默认诊断。拒绝 Electron basic_text 等
弱回退。静态加密保护不等于硬件隔离或用户在场认证。

安全存储不可用时禁用记住，可明确选择仅会话解锁。原生 secret handle 不可
序列化、Debug 脱敏。Node/FFI 用短生命周期受控 buffer，不用密码/密钥 JSON、
argv 或环境变量。应尽力清零，但不能承诺 GC/库/转储中全部副本可擦除。

状态为 Disconnected、Locked、Unlocking、Unlocked、Locking，账户授权独立。
锁定先关闭入口并使句柄失效，取消并等待持钥任务退出，然后释放/清零。后台任务
还持有可用密钥时不能宣称 Locked。显式锁定不得被 provider 自动加载绕过。
暂停保留密钥。断开/退出清理会话与任务，可保留原账户绑定的记住凭据，但在明确
重连/登录前禁用自动加载。忘记设备删除凭据。这些操作保留本地文件。

Provider 的加载、保存、删除任务必须留在会话任务边界内，即使调用方取消或退出。
加载的根密钥必须认证已绑定 config 并持久推进检查点，才能放行。缺少条目必须与
安全存储不可用或未授权区分。忘记设备必须先排空共享该条目的全部会话，删除退出
前阻止重连。删除失败不能显示已忘记；取消保存或删除调用不证明系统写入失败。
原生会话协调器必须阻止本进程中认证域、账户及仓库精确绑定相同的全部会话
重新准入，即使它们使用不同 provider 实例。删除退出前不能创建新会话、重连、
解锁或保存密钥。调用方在等待其他会话排空期间取消，也不能解除这项限制。
多个进程共享凭据条目时，宿主必须在删除前协调这些进程。

离线启动可以验证本机绑定/检查点，不宣称验证当前远端；第一次联网操作验证
当前 config。系统锁屏/休眠应锁定同步，恢复遵守保存策略。宿主区分本地访问和
需要解锁的 remote hydration。

## 16. 上限、错误与配额

| 项目 | 上限/默认值 |
| --- | --- |
| Config | 精确 206 bytes |
| Ref / 加密对象头 | 各 16 KiB |
| Hint | 1 MiB，4096 commits，32 parents/commit |
| 逻辑键 | 4096 UTF-8 bytes |
| Read/receive frame 数 | 256 |
| Read framed 响应 | 64 MiB |
| Fetch | 128 个 pack/index/hint tuple，总长 48 MiB |
| Fetch frame 数 | 384 |
| 批量 manifest | 256 KiB |
| 服务端图规划 | 每请求 100000 个不同 commit token |
| 数据并发 / KDF 并发 | 默认 4 / 每进程最多 1 |

Receive body 遵守 advertised 字节上限，大上传流式或 multipart，不整文件缓冲。
更小服务端上限优先，更大值不能解除本机上限。List 分页并检查键、重复和进展。
缓存与循环有预算和取消检查。服务端认证后才进行昂贵规划。

配额按实际密文和公开 hint。进度区分逻辑与线上字节。v1 不基于服务端推断可达性
自动删除已完成对象，失败 push 可留下孤儿。只自动过期未完成 multipart；长期
GC 需要单独定义客户端可达性/fencing 协议。

结构化错误包括 Locked、UnlockFailed、SecureStorageUnavailable、RememberedKeyMissing、
UnsupportedEncryptedProfile、RemoteIdentityChanged、RemoteConfigMissing、
ConfigRollbackDetected、CheckpointUnavailable、AuthenticationFailed、HintMismatch、HintUnavailable、
ImmutableConflict、StaleObservation、Conflict、OutcomeUnknown、MissingDependency、
QuotaExceeded、Cancelled。HTTP 回退状态不替代客户端区分。消息不含秘密，不
自动把解密路径/内容上传。

## 17. 初始化与恢复

服务端通过账户管理原子注册空远端身份/profile，账户 API 本身不由本文规定。
客户端先 create-if-absent 发布 config，再初始依赖、ref、HEAD，不覆盖并发结果。
只有初始分支可验证读取才显示已连接。
初始化发布必须拒绝指向不同提交的已观察分支，包括选定源提交的祖先；不得对其
应用普通 fast-forward 语义。分支不存在时允许 create-if-absent；已指向保留的
精确提交时，允许不替换 ref 的幂等恢复。

私有崩溃安全 journal 记录 UUID、候选 config 摘要/字节和 prepared、
config-published、objects-published、ref-published、connected 阶段。密钥只在
session/安全存储。竞争失败者丢弃未发布 root，显式接入胜出远端，不能把自己的
root 用在胜出身份下。崩溃后用保存密钥或密码解包同一 config，认证再恢复。
缺密钥不能用新 root 覆盖已发布 config。

原生密码准备流程必须区分显式创建与重开。创建必须拒绝已有的本地配置信任，
并在 journal 持久登记前认证生成的包装。重开必须解包保留的 generation-1
config，不能生成替代根密钥或配置字节；已有状态缺失或不一致时必须拒绝。
KDF、根密钥生成、验证和持久化必须留在解锁准入及锁定／排空边界内，即使
调用方取消。本地准备不发布 config，也不能表示仓库已连接。

绑定源的准备流程可以在登记初始候选时一并登记所选计划。此时，prepared 主记录
从首次可读版本开始就必须绑定私有计划的摘要；恢复时不得依据后来变化的 HEAD
补建计划。在任意已记录阶段重开，都必须保留精确候选，并要求保留计划与所选源
一致。重开后期阶段本身不证明远端效果。

首次发布数据前，可信宿主必须把所选规范分支名和权威源提交持久绑定到 journal。
这份私有计划在重试和重启后保持不变，不能从后来变化的本地 HEAD 重建。
保留计划缺失或不一致时，停止源发布和阶段推进。源变化必须在远端发现前拒绝，
并在发布依赖前再次检查。计划元数据位于仓库外，不进入插件存储或默认诊断包。

绑定初始化计划前，源仓库持久保存的远端配置必须与可信 authority 的预期配置
精确一致，并在发布时重新检查。独立验证使用另一份本地远端名称时，必须保持
相同的加密身份、端点、账户和会话生命周期；不能修改源注册或允许明文回退。

验证目标目录必须不存在或为空，且与源目录和宿主私有状态目录分离。宿主必须在
绑定初始化计划或发布远端数据前验证目标目录，并在实际克隆前再次检查。

Node 宿主通过 `initializeConfig` 的 `{ branch, commit }` 参数接收源选择，并在
发送配置前持久保存意图。未提供源意图的配置接入不能在之后发布初始分支；
`pushInitialBranch` 必须拒绝缺失意图，不能绑定调用时的 HEAD。

配置重试必须复用 journal 的精确字节。只确认配置不能显示已连接。响应丢失后，
读回并认证精确候选配置，才能标记 config-published。只有仍在 prepared 阶段时，
配置缺失才允许重试同一候选；config-published 后配置丢失必须失败，不重新初始化。
没有 mismatch 证据的不同配置保留结果未知。create-if-absent mismatch 加另一配置
表示竞争，但不认证胜出者，也不授权使用失败方的根密钥。发布前必须验证保留的
检查点，包括拒绝过时的 prepared journal。

阶段推进必须先确认对应远端效果，并在报告成功前持久化。持有密钥操作的 journal
持久化任务必须留在会话锁定与排空边界内，即使异步调用方退出。无法确认持久推进
时，初始化保持 pending，不授权生成替代配置。

忘记密码但有设备可取得 root 并完成本机认证，可原地重设包装。两者都丢失时，
可验证本地历史上传到新远端；只有当前文件只能恢复当前状态。只剩密文不能恢复。
客服/账户密码找回不提供解密后门。

## 18. 迁移与泄露后更换密钥

plain→encrypted 或 encrypted→新 root 必须新建 UUID/root 远端。选择全部历史
或指定 refs 的可达闭包，固定源 refs，获取验证全部 repository/storage/file
依赖，再加密发布目标 refs。重组 pack 可以改变布局，但不改变权威对象身份和
所选历史。缺依赖时完整迁移停止，不静默缩减为当前文件。

源选择必须保留附注标签的精确对象 ID，不能只保留其解引用后的提交目标。
显式选择 refs 时不能隐式加入无关的 HEAD 历史；可以单独固定 HEAD 来检测源
变化。源清单与仓库对象验证不能表示迁移完成：SQLite 底层存储和外部文件载荷
仍需验证，之后还需目标发布与独立恢复。重新验证 refs 只能检测变化，不能
替代最终写入隔离。

可恢复迁移必须在宿主私有状态中持久保留不可变源选择、精确 refs／HEAD、源身份
和依赖闭包指纹，并绑定到所选目标身份。恢复必须重新验证保留的源，不能换成
当前 HEAD 或重新选择的闭包。已分配记录缺失、损坏或身份不匹配时必须拒绝，
不能重建为新计划。源 journal 不能包含密码或根密钥，也不能证明目标已接入、
依赖可用或发布完成。

目标 ref 发布必须使用覆盖每个所选源 ref 的显式映射，保留权威附注标签 ID，
并与提交依赖区分。不能隐式映射远端跟踪名称；目标重名、文件／目录命名冲突
或分支／标签之间的类型转换必须在上传前失败。目标 HEAD 必须指向已映射分支或所选闭包
中的提交，不能扩大所选历史。仅有预检计划不能声称标签 wire 契约已实现或
发布成功。
全源清单必须把 symbolic HEAD 指向的未出生分支作为无目标状态保留，即使它
还没有 ref 文件。映射不能为它生成提交，也不能为缺失的 ref 上传 envelope。

目标发布前，精确映射和目标 HEAD 必须在宿主私有状态中持久绑定到冻结源记录
及所选目标身份。恢复必须沿用保留的意图，拒绝非规范／不一致状态并重新验证
源，不能根据当前默认值重新生成映射或 HEAD。源记录与目标记录必须可以区分。
意图不能包含密码或根密钥，也不能单独作为目标接入或发布确认的凭据。

发布迁移目标配置前，宿主必须把 generation-1 初始化候选的精确字节持久绑定
到保留的目标意图，包括其中的冻结源。恢复必须使用获准根密钥认证保留配置，
并要求初始化候选与目标意图完全相同。绑定缺失、损坏或不一致时必须停止发布，
不能据此生成替代根密钥或配置。这份私有绑定必须与源／目标计划区分，不能推进
配置信任，也不能算作远端接入凭据。协调器必须把准备过程排在远端初始化之前；
仅仅读取本机 prepared 阶段不能隔离并发初始化器。

迁移初始化必须在首次 HTTP 发现前验证保留的配置／目标／源绑定，并在记录已
确认的发布阶段前再次验证。持有密钥的验证任务必须留在会话锁定／排空边界内，
有界源遍历必须检查取消。远端效果发生后若验证失败，不能把它视为远端效果已被
撤销，也不能据此重新绑定；保留操作仍未确认。

依赖验证必须检查实际载荷的长度／内容及底层存储页面内容与所选身份是否一致。
缓存的存储哈希或已获取数据标记不能单独证明可用性或完整性。本地表示无法验证
时必须失败，不能静默替换源身份。

独立新会话完整 clone/hydrate 目标，对照 refs、对象身份、SQLite 快照、普通文件，
不能只依赖源缓存。容量估计包含密文和 hint。最终切换 fence/暂停旧写入口，补齐
最后修改，再验证固定 refs 并切换设备。缺服务 fence 时宿主协调明确暂停，披露
无法强制离线设备停止本地编辑。Journal 支持恢复，默认保留源。

旧远端删除独立授权。新加密不消除旧明文或旧 key 可读备份。泄露恢复还需撤销
实际服务凭据，只向可信设备分发新 root。只有账户级 token 时不得宣称单设备
撤销。可信客户端可以导入离线编辑并 merge，不给已撤销设备新 key。

## 19. 宿主、插件与发布契约

可信宿主应以一致名称提供设置密码、首次解锁、记住、重设、暂停、锁定、忘记
设备与迁移。正常记住设备不反复输入同步密码。成功 UI 依赖确认结果；不确定
更新显示 pending/unknown，不能显示完成。

普通插件 RPC 不暴露 KeyProvider 或改密码。插件只得到宿主授权的 Runtime
明文，不获取 root/wrapping key 或同步密码。数据与外部网络权限分开。同步插件
包/设置不自动授予设备执行或网络权限。解密后公开发布需要独立用户授权，不属于
远端保密承诺。

CLI 用无回显输入、受保护句柄或可信 provider。跨进程记住需要获准系统 provider，
否则逐会话解锁。CLI/SDK JSON 只返回状态和结构化结果。原生 SDK/宿主保持本文
契约，不能只在 JS fetch/push 外包装加密。

## 20. Conformance 与生产门槛

原生恢复存储必须在所有支持平台保留身份校验、互斥及持久替换。Unix 使用仅所有者
权限及 device/inode 身份。Windows 使用仅所有者/SYSTEM 的 DACL、volume/file
身份、拒绝 reparse point、原生文件锁、记录刷盘及同卷 write-through 替换。
ACL 或身份操作不受支持时必须拒绝。临时验证目录必须在使用前设置相同的私有
所有者及权限，使用提升权限的 Windows 进程创建时也不例外。Windows 口令输入必须关闭控制台回显，
并恢复原模式。记住密钥是可选能力；没有获准系统密钥 provider 的 Windows 宿主
使用显式口令进行仅会话解锁。

每次 HTTP 交换都必须具有有限的总时限，包括 aggregate、range、对象及 multipart
请求体和响应体，且同时保留连接与读取空闲时限。定期收到片段不能重置总时限。
原生数据交换默认总时限为 15 分钟，宿主可设置其他有限值；控制请求可更短。
修改请求超时后必须保留精确持久候选，并按结果未知的规则恢复。

GRAFT-E2EE-Remote-1.0 包含加密基础操作、key/config/object/ref/hint codec、
生命周期和已验证回退。GRAFT-E2EE-Sync-1.0 额外要求 fetch/receive 批量、增量
hint 索引与安全 multipart。记住/本机恢复声明需要分平台存储与认证证据。当前
实现未声明这些 profile。

必须提供：

1. 独立密码学评审；原语向量及可复现跨语言 config/object/ref/hint/route/framing
   编码向量。
2. Unicode/密码/KDF 上限、空对象、全部块长/range 边界、跨 key/仓库替换、
   prefix/header/chunk/ref/hint 篡改与截断。
3. 伪造 hint、parent 冲突、多 pack 相同提交、错误 pack 归属、环、缺图、旧缓存
   和重建；提示不改变已验证 fetch 结果，不跳过依赖验证。
4. 同/不同 ref 并发、删除重建、改密码竞争、同明文不同密文碰撞、部分 receive、
   multipart 身份不匹配、响应丢失、不确定结果与取消。
5. 初始化/密码更新/迁移每阶段 kill/reopen、锁定/stale handle、不支持版本与
   禁止明文降级。
6. 真 CLI/Node 和所声明宿主上的 SQLite/WAL、普通文件、历史、clone/hydration、
   merge 与活跃句柄 worktree 安全，不能用 mock 取代。
7. 忘记密码本机恢复、不安全存储拒绝、账户切换、退出/忘记、系统锁屏恢复、
   真正独立的目标恢复。
8. 用独特合成明文标记扫描请求/存储/日志/诊断，不得有 secret、原始 hash、
   名称或内容。预期公开拓扑单独记录验证，不能标成图关系零泄露。
9. 同条件 plain/encrypted 基准：首 clone、冷/热小更新、长离线 pull、4 KiB
   hydration、大附件、10 万对象发现、分支竞争。报告 P50/P95、请求数、线上
   字节、读放大、CPU、峰值内存、KDF 延迟、孤儿占用；比较 16/64/256 KiB。
   不宣传未经测量的变慢百分比。

生产门槛：流式内存有界，普通已缓存小更新不全仓库重加密/重扫，无全局 ref 锁，
仅接受已认证数据，确认发布可恢复，并根据 plain 实测基线设性能预算。公开独立
离线验证工具和向量。一次抓包不证明整个客户端永远安全。

本机配对回归政策独立于线协议版本。
`scripts/e2ee-performance-policy.json` 记录
`graft-e2ee-performance-policy-v2`；仅对 core 的 `initial_push`、
`cold_clone_sync`、`long_offline_push` 和 `long_offline_fetch` 应用：

- 加密 P50 不得超过 `max(3 × plain P50, plain P50 + 500 ms)`。
- 加密 P95 不得超过 `max(3 × plain P95, plain P95 + 750 ms)`。

core 热小更新保留 500 ms P95 上限。SDK/hydration 耗时预算、请求数、线上字节、
RSS、孤儿存储门槛及采样数不变：plain-first 和 encrypted-first 各运行三轮 core、
五轮 SDK/hydration。完整矩阵为 498 项，包含新增的 18 项 core bulk P50 检查。
百分位采用 nearest-rank；有限样本属于回归证据，不是生产尾延迟 SLA。
源码、汇总和验收记录必须标识政策版本。历史失败保留原预算和结果；政策变化
需要重新完整运行，不得重标历史证据或丢弃失败样本。

设计定稿为 1.0；互操作需要精确向量和上述评审。编码、泄露范围、算法、原子
语义的变化要明确升级版本/profile，不能无记录修改。

## 21. 参考

- [RFC 9106](https://www.rfc-editor.org/rfc/rfc9106.html)：Argon2id 与参数。
- [RFC 5869](https://www.rfc-editor.org/rfc/rfc5869.html)：HKDF。
- [Libsodium XChaCha20-Poly1305](https://doc.libsodium.org/secret-key_cryptography/aead/chacha20-poly1305/xchacha20-poly1305_construction)：AEAD/nonce 原语要求，不代表本文协议获背书。
- [Graft Remote Sync 1.0](./graft-remote-sync-1.0.md)：继承的同步安全。
