# Graft Remote changelog

## Graft Remote 0.2.5 — 2026-10-16

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
