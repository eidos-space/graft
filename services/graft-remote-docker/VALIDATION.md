# Local validation / 本地验证

Historical first-stage evidence / 第一阶段历史证据。当前状态见 [DELIVERY.md](DELIVERY.md)。

Date / 日期: 2026-11-03

| Evidence / 证据 | Result / 结果 |
| --- | --- |
| Backend and management unit tests / 后端及管理单测 | 4 passed; includes atomic create/CAS, pagination, validation, authentication, identity persistence / 4 项通过，覆盖原子写入、分页、校验、认证及身份持久化 |
| Shared remote engine / 共享协议引擎 | 47 tests passed / 47 项通过 |
| Docker build and Compose / 镜像与 Compose | Passed on Linux ARM64 via OrbStack, Node 24.22.2; non-root, read-only filesystem, secret file and named volume / 非 root、只读根目录、秘密文件及命名卷通过 |
| HTTP / HTTP 接口 | Health, unauthenticated 401, unknown repository 404, spoofed Host/Forwarded cannot change discovery origin / 健康、未认证拒绝、未知仓库及伪造转发头检查通过 |
| Durability / 持久化 | Plain upload survives SIGKILL and container recreation; stopped tar backup restored into a newly created empty volume / 普通上传在强杀及重建后保留；停机 tar 备份恢复至新空卷通过 |
| Native encrypted SDK / 原生加密 SDK | Published `@eidos.space/graft` 0.3.37 installed in the Lite checkout; initialization, verified first push, wrong-password rejection, independent clone after SIGKILL, SQLite and attachment update/push/pull passed / 使用 Lite 检出中安装的已发布 0.3.37，初始化、首次推送验证、错误密码拒绝、强杀后独立克隆及数据库和附件双向同步通过 |
| Lite source client / Lite 源码客户端 | Actual `CustomSyncClient` loaded with Bun: discovery, invalid key, creation, descriptor UUID, rename and listing passed; no profile opened / Bun 加载实际类，发现、错误密钥、创建、UUID、改名及列表通过，未打开用户配置 |
| Documentation / 文档 | Astro build, generated installer comparison and shell syntax passed / Astro 构建及安装脚本一致性、语法检查通过 |

The isolated container test used a unique Compose project and temporary access key.
Its containers, volume, key and per-run image were removed. No production service
or real user Space was touched. No image was published and no deployment occurred.

容器测试使用独立 Compose 项目及临时密钥；容器、数据卷、密钥和当次镜像已
删除。没有修改生产服务或真实用户 Space，没有发布镜像或部署。

## Limits of this evidence / 证据边界

- A health check is not a sync test. Native encrypted synchronization and Lite's
  source management client were tested separately.
- Packaged Lite/Electron UI interaction, real mobile devices, public TLS/certificate
  configuration, x86_64 containers, long-running load and large-Space performance
  were not validated in this run. The added GitHub workflow has not run remotely.
- That first-stage server was single-instance, with a 64 MiB request/object ceiling and no
  multipart capability; the later candidate adds multipart and needs its own evidence. No claim is made for all existing user Space sizes.
- The SDK package initially present in this Graft checkout had a plain native
  binary. The successful encrypted run explicitly selected Lite's published
  0.3.37 package; a JavaScript E2EE export alone is not proof of native support.

健康检查不代表同步成功，原生加密同步和 Lite 源码管理客户端是分别测试的。
本次未验证已打包的 Lite/Electron 界面、真实移动设备、公网证书配置、x86_64
容器、长期负载或大型 Space 性能；新增 GitHub 工作流尚未在远端运行。
当时实现为单实例、单请求/对象 64 MiB、无 multipart；后续候选已加入分片，需独立证据，不宣称适用于任意
大小的现有 Space。Graft 检出中原有原生 SDK 未启用 E2EE，成功测试明确选用
Lite 安装的已发布 0.3.37；仅有 JavaScript 导出不证明原生支持。

## Later Docker candidate / 后续Docker候选

Published SDK 0.3.37 was rerun against the multipart/deletion candidate with an
80 MiB random attachment: full initialization restore, independent post-SIGKILL
clone, SQLite/attachment updates and deletion restart/idempotence passed. Log:
`/tmp/graft-docker-large-037.log`. This is old-client compatibility and large-file
recovery evidence, **not** proof that the new default initialization avoids payload
readback. The failed new-SDK 80 MiB sample and revision-1 fix status are retained
separately in DELIVERY.md. No native Linux AMD64 execution or public image release
is inferred from this local Linux ARM64 run.

已发布0.3.37对新Docker的大附件全量恢复和删除恢复通过；新SDK轻量路径此前
80MiB失败样本另行保留。不能用旧客户端全量下载成功替代新流程零负载回读证据。
