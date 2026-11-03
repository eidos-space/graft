# 使用 Docker 自托管 Graft

[English](README.md)

为 Eidos Lite 和原生 Graft SDK 提供个人同步服务器。复用仓库内
`packages/graft-remote` 的 Remote v1 和加密同步协议，同时实现 Lite 所需的
服务发现、仓库创建、列举和显示名称修改。不依赖 Eidos 账号、账单服务、
Cloudflare 或外部数据库。

## 启动

需要 Docker Engine 和 Compose v2。在本仓库检出目录中执行：

```sh
cd services/graft-remote-docker
mkdir -p secrets
chmod 700 secrets
(umask 077; openssl rand -hex 32 > secrets/access-key)
# Compose 绑定宿主机文件，非 root 容器需要读取权限。
# 外层目录保持私有，不要提交密钥文件。
chmod 444 secrets/access-key
# 仅用于本地 SDK 测试，Lite 常规界面要求 HTTPS。
printf 'GRAFT_PUBLIC_ORIGIN=http://127.0.0.1:8787\n' > .env
docker compose up --build -d --wait
curl --fail http://127.0.0.1:8787/healthz
```

容器以 UID/GID 1000 运行，根文件系统只读，移除 Linux capabilities，
数据存于 `/data` 对应的 `graft_data` 持久卷。默认端口只绑定宿主机回环地址。
`restart: unless-stopped` 会在进程失败或宿主机重启后恢复；健康检查失败本身
不会触发 Docker 重启。需要保留数据时，不要执行 `docker compose down -v`。

访问密钥具有所有仓库的完整权限，请存入密码管理器并填入 Lite。
它与仓库的**加密密码**不同；加密密码由客户端处理，不应发送给管理 API。
服务器启动时读取 `/run/secrets/graft_access_key`，没有公开初始化页面、
默认密码或环境变量密钥回退。轮换时用新的随机密钥替换 `secrets/access-key`，
恢复权限，执行 `docker compose up -d --force-recreate`，并更新所有客户端。
仓库身份和加密密码不变。

## HTTPS 与 Eidos Lite

将专用域名（例如 `sync.example.com`）指向服务器，将 `.env` 改为：

```dotenv
GRAFT_PUBLIC_ORIGIN=https://sync.example.com
```

在宿主机运行 Caddy，确保 80/443 端口可达，Caddyfile 为：

```caddyfile
sync.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

Caddy 终止 TLS 并获取证书。修改 `.env` 后重建容器。保持 8787 端口私有。
如果代理也运行在 Docker 内，将它连接到同一 Compose 网络并使用
`graft:8787` 作为上游；独立代理容器的 `127.0.0.1` 不指向 Graft。
代理需允许最多 64 MiB 请求和最长 120 秒传输，保留 Authorization、
Graft-Protocol、Content-Length 及协议头，不改写路径、转换响应、缓存或
重定向 API 请求。不要记录 Authorization 或请求体。

服务只根据 `GRAFT_PUBLIC_ORIGIN` 生成发现信息和仓库 URL，不信任
Host/Forwarded 头。TLS 由代理提供，容器私有接口本身运行 HTTP；不要将其
直接暴露给不可信网络。公开地址只能是 origin，不能包含路径、查询或凭据。
仅回环地址允许 HTTP 开发测试。

在 Lite 的自定义同步服务器设置中添加 `https://sync.example.com` 和访问
密钥，再创建或选择远程仓库，按客户端加密密码流程连接。这是独立远程：
直接修改已有连接的服务器地址不会迁移原来的加密身份或数据。

## API 与限制

| 方法 | 路径 | 认证 |
| --- | --- | --- |
| GET | `/`、`/healthz`、`/.well-known/graft` | 公开 |
| GET | `/api/graft/repositories` | Bearer 密钥 |
| PUT | `/api/graft/repositories/:name` | Bearer 密钥 |
| PATCH | `/api/graft/repositories/:name` | Bearer 密钥 |
| DELETE | `/api/graft/repositories/:name` | Bearer 密钥 |
| GET | `/api/graft/repositories/:name/status` | Bearer 密钥 |
| 协议方法 | `/personal/:name/...` | Bearer 密钥及 `Graft-Protocol: 1` |

PUT 接受 `{"display_name":"笔记","encryption":"encrypted-remote-v1"}`，
空请求体默认创建加密仓库。普通 SDK 可显式指定 `"plain"`。
PATCH 仅接受 `display_name`。技术名称为 1–64 个 ASCII 字母、数字、点、
下划线或连字符，首尾必须为字母或数字。显示名称会去除首尾空白并 NFC
规范化，长度 1–80 个 Unicode 码点，不含控制字符。管理 JSON 最多 4096
字节，拒绝未知字段。

创建可幂等重试（首次 201，已有 200），不能改变已有仓库的加密类型（409）。
加密 UUID 持久保存。未知仓库返回 404，不会隐式创建。
响应遵循个人 Cloudflare 管理接口，包含 `namespace: "personal"`、
`remote_url`、毫秒时间戳 `created_at` 和加密描述符；列表为
`{namespace, repositories}`。PATCH 只修改显示标签。

这是**单实例个人服务器**，不是多租户服务。默认本地模式下，SQLite WAL 和 FULL 同步模式将
目录、身份及不透明同步对象存入同一数据库；对象发布和比较交换由原子 SQL
完成。重启后可重试未确认请求，加密路由索引由共享引擎从持久提示对象重建。
每个卷只运行一个实例，使用本地磁盘而非 NFS，并监控剩余空间。
支持显式仓库删除；不提供通用可达性垃圾回收、存储配额、账号角色或复制。

单次 HTTP 请求最多 64 MiB，同时最多处理 4 个 HTTP 请求，超出返回带
Retry-After 的 503。后端缓冲单个对象，使用同步 SQLite，大写入会阻塞其他
操作。Compose 限制内存为 1 GiB。支持加密及普通 multipart，分片为 16 MiB，协议上限为 10,000 片。
分片存于 SQLite，重启可续传；完成时流式组装到私有文件，校验加密对象的
完整摘要和上传身份，同步文件和目录后再原子发布 SQLite 元数据。大对象读取
使用文件流。Space 容量不等于单请求大小，仍受磁盘与客户端限制；实际大附件
覆盖见验证记录，理论分片上限不代表规模测试通过。

## 备份、恢复和升级

备份时记录源码版本和镜像身份。停止服务后备份整个 `/data`；运行时只复制
SQLite 主文件可能丢失 WAL 数据。在本目录执行：

```sh
mkdir -p backups
chmod 700 backups
docker compose stop graft
docker compose run --rm --no-deps -T --entrypoint tar graft -C /data -czf - . > backups/graft-data.tar.gz
chmod 600 backups/graft-data.tar.gz
docker compose start graft
```

将备份安全保存到其他机器：仓库标签及显式创建的普通仓库并未加密。
单独备份访问密钥并保管客户端加密密码，服务器备份不能恢复这些密码。
恢复时停止目标服务，使用**空数据卷**并保留 UID 1000 的所有权。
在 Compose 已创建目标卷的情况下执行：

```sh
docker compose run --rm --no-deps -T --entrypoint tar graft -C /data -xzf - < backups/graft-data.tar.gz
docker compose up -d --wait
```

恢复后核对仓库列表、UUID 和客户端下载，再恢复使用。不要将备份覆盖到
运行中或非空的数据库目录。

升级前停止并备份，审查变更，检出指定 Graft 版本，执行
`docker compose build --pull` 和 `docker compose up -d --force-recreate --wait`。
验证前保留旧镜像和备份。启动会检查存储版本 4（本地）或 5（S3），遇到未知版本拒绝运行。
跨存储迁移的回滚需要恢复匹配的升级前备份。仓库不自动拉取或发布服务镜像，
Dockerfile 默认安装 `REMOTE_VERSION` 固定的正式 npm Remote 包，并按
`REMOTE_SHA256` 校验归档。Node 基础镜像已固定到审核过的 digest。
只有显式指定 `GRAFT_REMOTE_SOURCE` 的候选测试才从源码构建协议引擎。

## 验证

在仓库根目录安装 pnpm 依赖后执行：

```sh
npm ci --prefix services/graft-remote-docker --ignore-scripts
pnpm test:docker
# 需要 Docker，以及 packages/graft-sdk 中已构建且支持 E2EE 的 SDK：
pnpm test:docker:container
# 也可指定另一份已构建 SDK
GRAFT_SDK_PATH=/absolute/path/to/sdk pnpm test:docker:container
```

容器脚本创建独立命名的 Compose 项目、临时访问密钥和数据卷，在 finally 中
删除测试数据。验证真实镜像构建启动、健康、未授权拒绝、固定发现地址、普通
上传、SIGKILL 后恢复、容器重建、停机备份和空卷恢复，以及原生 SDK 加密初始化与推送、错误密码
拒绝、崩溃后克隆、SQLite 和附件双向更新。不会连接生产服务器。
`GRAFT_TEST_SKIP_SDK=1` 只测试容器和协议，不构成 E2EE 验证。

健康检查证明进程和数据库可读；SDK 测试证明原生协议互操作。两者都不能单独
证明已发布 Lite 的 UI 或运营者的公网 HTTPS 配置可用。
详见[验证记录](VALIDATION.md)。

本地 SDK 若未启用 E2EE，可显式构建：

```sh
bash packages/graft-sdk/scripts/build-native.sh "$(rustc -vV | sed -n 's/^host: //p')" packages/graft-sdk/native e2ee
```

如有 Lite 源码检出和 Bun，可将 `GRAFT_LITE_CLIENT_PATH` 设置为其
`src/main/sync/custom-sync-client.ts`，容器测试将调用该实际类验证发现、错误
密钥拒绝、创建、描述符、改名和列表，不打开用户配置。这仍是源码客户端
测试，不是已打包 Electron UI 验证。

存储提交与响应顺序，以及进程恢复和断电安全的区别，详见
[存储审查与交接](STORAGE-REVIEW.md)。

## 改名与删除

发现信息声明 `management.version: 1` 和 `capabilities: ["rename", "delete"]`。
DELETE 返回 `{namespace, repository, status}`；处理中 `deleting` 为 202，
完成 `deleted` 为 200。GET `.../:name/status` 查询同一状态，重复删除幂等。
列表包含 active/deleting，移除已完成项。删除开始后新协议请求返回 410；
已准入响应流完成或取消后才进行物理清理。分批清理在启动时恢复，I/O 失败时
保持 deleting。

**技术名称和 URL 永不复用**。删除后的名称 PUT 返回 409；显示名称可以通过
新的技术名称重复使用。这样旧上传及旧删除重试不会命中新仓库。删除不可逆，
删除的是服务器副本，不会直接删除设备本地文件。

## GitHub 发布部署（首次发布待完成）

计划镜像为 `ghcr.io/eidos-space/graft-remote-docker`，独立标签为
`graft-docker-vX.Y.Z`。发布后从对应 GitHub Release 下载 `compose.yaml`、
`SHA256SUMS`、`image.json`、`SOURCE_COMMIT`。Compose 资产固定镜像 digest。
在私有目录中保存资产，按前文创建 `secrets/access-key` 和 `.env`，再执行
`docker compose pull` 和 `docker compose up -d --wait`，无需构建源码。
升级先停机备份，保留旧 Compose，校验新发布资产后拉取并重建；不要换成 latest。

发布要求 Linux 两种原生架构验收、`SDK_VERSION` 中固定的公开 SDK 复验，以及
已合并的 release PR。本地源码客户端或 macOS addon 测试不能代替这些门槛。
GHCR 首次包需可公开读取，匿名拉取是正式验收条件。

## 候选联测

未发布 SDK 与旧客户端全量验证分开运行：

```sh
GRAFT_TEST_SKIP_SDK=1 \
GRAFT_CANDIDATE_SDK_PATH=/absolute/candidate/packages/graft-sdk \
GRAFT_CANDIDATE_NATIVE_PATH=/absolute/candidate/graft-sdk.darwin-arm64.node \
GRAFT_LARGE_PAYLOAD_MIB=80 pnpm test:docker:container
```

记录 native SHA-256、镜像 ID/架构、成功负载读取、404 探测和字节数；验证默认
发布确认、显式完整恢复、独立克隆和故障恢复。`GRAFT_CANDIDATE_SCENARIOS` 可
选择逗号分隔的场景用于排查，不代表完整矩阵通过。`GRAFT_TEST_IMAGE` 可直接
测试已构建/已拉取镜像，避免重新构建。

未完成multipart在空闲七天后过期，已准入操作受保护。预留分片、组装文件和
SQLite WAL的临时磁盘空间。删除是逻辑移除，不承诺取证级擦除或自动压缩数据库。
对象校验后端读取真实持久密文，最多两次并发、60秒截止，响应HTTP取消；须接入
共享Remote 0.2.6 handler后才声明新能力。后端单测不等于原生端到端校验通过。

## S3 兼容存储：Backblaze B2 与 Cloudflare R2

默认仍为本地磁盘（`GRAFT_STORAGE=local`）。叠加 `compose.s3.yaml` 可连接已有
bucket：不可变对象及完成上传的大文件密文存入 S3，SQLite 保留仓库身份、引用、
配置、对象指针、删除状态和未完成的上传分片。因此仍必须持久化 `/data`，并为
上传分片和每个并行完成操作的完整组装文件预留临时磁盘。S3 上传成功后会删除
本地组装文件并清理分片记录；SQLite 可能保留已分配的空闲页或 WAL 空间供复用，
数据卷不一定立即缩小。这不是无状态服务，也不是客户端直传 bucket。

1. 创建专用 bucket 或独占前缀。B2 使用限制到该 bucket、具备读写删除权限的
   **标准 application key**，并从 bucket 详情复制 S3 endpoint 和对应 region。
   参见 [B2 官方 SDK 配置](https://www.backblaze.com/docs/cloud-storage-use-the-aws-sdk-for-javascript-v3-with-backblaze-b2)
   和 [应用密钥说明](https://www.backblaze.com/docs/cloud-storage-application-keys)。
2. 将 `.env.s3.example`（Release 资产名为 `env.s3.example`）复制为 `.env`，替换服务地址、endpoint、region、bucket。
   保留本地模式部署步骤中创建的 Graft 访问密钥。
3. 把 B2 `keyID` 写入 `secrets/s3-access-key-id`，`applicationKey` 写入
   `secrets/s3-secret-access-key`。沿用访问密钥的权限：`secrets` 目录 0700，
   挂载文件 0444。凭证通过秘密文件传入，不写入 SQLite、远端 URL 或结果。
4. 使用**新的数据卷**启动：

   ```sh
   docker compose -p graft-s3 -f compose.yaml -f compose.s3.yaml up --build -d --wait
   ```

使用 GitHub Release 时，下载其 `compose.yaml`、`compose.s3.yaml` 和
`env.s3.example`，并核对 `SHA256SUMS`。该 `compose.yaml` 已固定镜像 digest，
使用上面的命令时去掉 `--build` 即可。若使用源码中的 `compose.release.yaml`
模板，则需设置固定 digest 的 `GRAFT_IMAGE`。本次源码变更尚未发布为镜像。

后续停止、启动、备份、恢复和升级等所有 Compose 命令，都保留
`-p graft-s3 -f compose.yaml -f compose.s3.yaml`。遗漏项目名或叠加配置可能选中
其他数据卷或以错误的存储模式启动。

| 配置 | 用途 |
| --- | --- |
| `GRAFT_STORAGE` | `local`（默认）或 `s3` |
| `GRAFT_S3_ENDPOINT` | HTTPS origin，例如 `https://s3.us-west-004.backblazeb2.com` |
| `GRAFT_S3_REGION` | 与 endpoint 匹配的区域 |
| `GRAFT_S3_BUCKET` | 已有 bucket；服务不会创建 bucket |
| `GRAFT_S3_PREFIX` | 非空独占前缀，以 `/` 结尾，默认 `graft/` |
| `GRAFT_S3_FORCE_PATH_STYLE` | `true`（默认）或 `false` |
| `GRAFT_S3_ACCESS_KEY_ID_FILE` / `GRAFT_S3_SECRET_ACCESS_KEY_FILE` | 挂载的凭证文件 |
| `GRAFT_S3_SESSION_TOKEN_FILE` | 可选的 AWS 临时会话 token 文件 |

Cloudflare R2 使用仅限目标 bucket 的 **Object Read & Write** token，配置如下，
不需要部署 Worker：

```dotenv
GRAFT_S3_ENDPOINT=https://<account-id>.r2.cloudflarestorage.com
GRAFT_S3_REGION=auto
GRAFT_S3_BUCKET=your-dedicated-bucket
GRAFT_S3_PREFIX=graft/
```

R2 沿用同样的 secret 文件与 Compose 叠加配置。其 S3 API 不支持 bucket 版本化，
删除会直接移除当前对象；版本化 S3 服务则通过 VersionId 删除。R2 默认在七天后
中止未完成的 multipart 上传。参见 [R2 S3 API 兼容说明](https://developers.cloudflare.com/r2/api/s3/api/)
和[分片上传生命周期](https://developers.cloudflare.com/r2/objects/upload-objects/)。

使用 SigV4、Get/Head/Put/DeleteObject、Create/Upload/Complete/AbortMultipartUpload
及 HeadBucket。需要对象读写删除权限；开启版本管理时还需删除对象版本权限，
HeadBucket 需要 bucket 访问权限。不需要列出账户全部 bucket 或创建 bucket 的权限。
启动时检查连接；远端故障不会静默回退本地。HTTP endpoint 默认拒绝，只有明确设置
`GRAFT_S3_ALLOW_HTTP=true` 才允许，供隔离本地测试服务使用。

S3 确认对象上传完成后才在 SQLite 发布指针，引用通过本地事务更新。proof 流式读取
实际 S3 对象，不使用上传时缓存的摘要。删除等待已准入读者结束，再清理登记的远端
对象；失败保持 `deleting` 并重试。版本化对象按 VersionId 删除，仍受 retention/
Object Lock 权限约束。不要从外部改写此前缀或添加删除标记。请在提供商端配置清理
未完成 S3 multipart 的规则：进程崩溃可能留下 SDK 尚无法恢复 ID 的远端上传会话。
不要给仍在使用的对象配置自动过期。

**备份必须同时包含 `/data` 和对应 bucket 对象**，并在停止写入后协调获取。
仅有 bucket 不能重建仓库引用；删除对象后仅恢复旧 SQLite 也不能恢复这些数据。
一个数据目录只能运行一个服务，共享 bucket 不代表支持多实例集群。

数据目录会绑定模式、endpoint、region、bucket 和 prefix，防止修改配置后悄悄读错
存储。尚未实现已有本地数据自动迁移到 S3 或跨 bucket 迁移；迁移时保留原服务与卷，
建立新实例，再通过客户端同步数据。同一后端可以替换凭证文件并重启。

验证先执行 `npm ci --prefix services/graft-remote-docker --ignore-scripts`，再运行
`pnpm test:docker`。指定隔离 MinIO 镜像后可运行
`GRAFT_S3_TEST_IMAGE=<image> node --test services/graft-remote-docker/test/s3.integration.mjs`。
完整容器脚本支持 `GRAFT_TEST_S3=1 GRAFT_S3_TEST_IMAGE=<image>`，只创建自己的 bucket
和随机测试凭证，不使用已有用户 bucket。MinIO 验证通过不等于真实 B2 部署验证。
