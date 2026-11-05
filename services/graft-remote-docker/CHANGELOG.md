# Graft Docker releases

## 0.2.0

- Browser setup in English and Simplified Chinese replaces manual access-key generation for new installations.
- Choose local storage or configure and test S3-compatible storage in `/setup`.
- One-time initialization links, persistent private configuration, explicit key recovery and closed setup after completion.
- Optional Caddy HTTPS overlay; existing proxies and external-key deployments remain supported.
- Native container acceptance now covers browser-setup APIs, restart persistence and public SDK encrypted synchronization with local and S3 storage.

## 0.2.0（中文）

- 新安装通过中英文网页初始化，无需手动生成访问密钥。
- 在 `/setup` 中选择本地存储，或配置并测试 S3 兼容存储。
- 一次性初始化链接、持久化私有配置和密钥取回命令；完成后关闭初始化入口。
- 可选 Caddy HTTPS 配套配置，继续支持已有代理及外部密钥部署。
- 原生容器验收覆盖初始化 API、重启持久化，以及本地和 S3 存储的公开 SDK 加密同步。

## 0.1.1

- Verify the remote annotated release tag independently of checkout’s local ref. The failed 0.1.0 tag produced no image or Release.
- Optional S3-compatible/B2 bucket storage with secret-file credentials; local disk mode remains the default.
- Personal self-hosted Remote v1 and encrypted sync, with Eidos Lite discovery and management.
- Secret-file authentication, non-root Compose deployment, persisted SQLite identities and refs.
- Durable multipart uploads with streamed ciphertext verification and file publication.
- Explicit rename/delete capabilities, permanent technical names, deletion tombstones and resumable cleanup.
- Isolated native SDK round trips, fault injection, large attachments, backup and restore validation.

## 0.1.1（中文）

- 独立校验远端 annotated tag，兼容 checkout 展开本地 tag 的行为；失败的 0.1.0 tag 未产生镜像或 Release。
- 可选 S3 兼容/B2 bucket 存储与秘密文件凭证，本地磁盘仍为默认模式。
- 个人自托管 Remote v1 和加密同步，包含 Lite 服务发现与仓库管理。
- 秘密文件认证、非 root Compose 部署、SQLite 持久身份及引用。
- 持久分片上传、流式密文校验与文件发布。
- 显式改名/删除能力、永久技术名称、删除墓碑和可恢复清理。
- 隔离原生 SDK、故障注入、大附件及备份恢复验证。
