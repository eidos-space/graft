# Graft Remote changelog

## Graft Remote 0.2.6 — 2026-11-03

- Add the optional `e2ee-object-proof-v1` capability and bounded integrity challenge endpoint. Backends opt in with `encryptedObjectProof`, hashing the supplied domain-separated prefix and every byte of the actual immutable stored ciphertext snapshot. The handler validates framing, identity, length and digest shape, and authorizes the operation as a read.
- Hosts must retain repository admission and a consistent snapshot throughout hashing, implement cancellation and resource limits, and reject tombstoned repositories. Upload-time hashes and cached answers do not satisfy this contract. Existing clients and backends without this capability remain supported.
- This confirms current ciphertext integrity against a client's retained candidate; it does not establish future durability or independent restore verification.

### 中文

- 新增可选的 `e2ee-object-proof-v1` 能力及有界完整性挑战接口。后端通过 `encryptedObjectProof` 接入，对带域隔离的前缀和实际不可变存储快照的全部密文字节计算摘要。处理器验证请求格式、身份、长度和摘要格式，并按读取操作鉴权。
- 宿主须在完整计算期间持有仓库准入和一致快照，提供取消及资源限制，并拒绝已删除仓库。上传时缓存的摘要或缓存响应不能替代实际读取。旧客户端和未实现该能力的后端继续兼容。
- 该接口核实当前密文与客户端保留候选是否一致，不证明未来持久性或独立恢复能力。

## Graft Remote 0.2.5 — 2026-10-17

### Added

- The framework-neutral Remote handler supports the encrypted remote profile, including encrypted object transfers, conditional mutable-object updates, routing hints and encrypted bundles. Encrypted repositories reject plain semantic operations and require secure transport.
- Cloudflare hosts can explicitly register an encrypted repository with `createEncryptedCloudflareRepository`, reopen its persisted profile with `openCloudflareRepository`, and rebuild encrypted routing metadata with `rebuildCloudflareEncryptedRouting`.
- Core, Hono and Cloudflare packages remain one versioned cohort. Release verification checks the encrypted exports in public installations, and GitHub releases use a committed version-specific description instead of an automatically generated comparison link.

### Compatibility

- Existing plain Remote services remain supported. Encrypted creation is explicit; upgrading these packages does not convert existing repositories or issue account credentials.
- The host service remains responsible for authentication, authorization, repository registration, quotas and deployment policy. It stores encrypted payloads and routing metadata; client password recovery and content verification remain client responsibilities.
- Deploy encrypted-capable server support before allowing clients to create encrypted repositories. Keep persistent encrypted profile state and retained object data during rollback or maintenance.

### 中文

- 通用 Remote 处理器新增加密仓库协议，支持加密对象传输、带条件的可变对象更新、路由提示和加密批量传输。加密仓库拒绝普通仓库的语义操作，并要求安全传输。
- Cloudflare 宿主可通过 `createEncryptedCloudflareRepository` 显式创建加密仓库，通过 `openCloudflareRepository` 按持久化的仓库类型重新打开，并使用 `rebuildCloudflareEncryptedRouting` 重建加密路由元数据。
- Core、Hono 和 Cloudflare 三个包保持同版本发布。发布验证检查公开安装包的加密 API，GitHub 发布说明必须来自仓库中对应版本的正文。
- 现有普通仓库继续兼容，升级不会自动转换仓库。账户认证、权限、仓库登记、配额和上线策略由宿主服务负责，密码恢复及内容校验由客户端负责。
- 应先部署支持加密协议的服务，再允许客户端创建加密仓库。回滚和维护时需保留已登记的加密仓库类型及相关持久化数据。
