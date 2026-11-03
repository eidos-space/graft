# Storage review and management handoff

Date: 2026-11-03. This reviews the implemented Docker backend, including multipart files, not an independent filesystem-object database. No new wire capability or SDK version gate is needed
for publication confirmation.

## Publication and acknowledgement

- `storage.mjs` stores the directory, ordinary immutable objects and mutable refs in one
  SQLite database, with immutable multipart files on the same mounted volume. Each connection enables WAL,
  `synchronous=FULL`, foreign keys and a busy timeout.
- `putIfAbsent` consumes and validates the complete bounded stream before its
  atomic `INSERT OR IGNORE`. An interrupted object is not visible. A complete
  prefix of an interrupted aggregate may remain orphaned and can be retried.
- `compareAndSwap` uses a single conditional SQL update (or create-only insert).
  Another connection cannot win with the same stale expected value. Metadata
  provisioning and profile/UUID identity are also persisted before success.
- These synchronous statements finish before the backend returns. The shared
  protocol engine awaits each immutable frame and stream completion before its
  aggregate ref CAS; only afterward does it create the successful response.
  Individual requests rely on the conforming SDK awaiting dependency publication
  before submitting refs/HEAD. The opaque server cannot prove arbitrary client
  ref reachability and does not authenticate/decrypt client content.
- Losing a response does not roll back an already committed statement. Retrying
  immutable creation or a ref CAS must reconcile existing state; it must not
  overwrite different immutable bytes or a competing ref. SDK fault tests use
  real Docker HTTP responses and check independent post-restart recovery.

SQLite documents that FULL in WAL mode synchronizes the WAL for each transaction
commit ([SQLite synchronous](https://www.sqlite.org/pragma.html#pragma_synchronous)).
This design depends on the operating system, VM, filesystem and device honoring
those synchronization requests. The tests kill the process/container; they do
**not** simulate power loss, torn sectors, disk-controller cache failure or host
filesystem corruption. Atomic rename alone would not establish those guarantees,
and this backend uses file/directory synchronization followed by SQLite publication for multipart files.

The aggregate protocol test uses public synthetic framing plus the real SQLite
backend and an SQL trigger to check that both dependencies exist before ref
insertion. It is a storage ordering test, not cryptographic E2EE evidence. Native
SDK tests provide the separate encrypted round trip.

## Deletion contract implemented

The frozen management contract permanently reserves technical names/URLs. This
removes the old-name-reuse hazard without inventing a generation identity in the
wire API. Directory tombstones are never deleted or reclassified. New display-label
reuse requires a new technical name and encryption UUID.

A request admits the active repository synchronously and retains a lease through
response completion or cancellation. DELETE first commits `deleting`, rejects new
admissions, and leaves already-admitted readers intact. Every mutation rechecks
active state after asynchronous body/file work; stale uploads cannot publish.
Cleanup waits for leases, then deletes bounded batches of objects/uploads/files;
only an empty repository becomes `deleted`. Failure leaves `deleting`, and startup
continues it. File names are generated upload UUIDs, never user path components.

## Multipart files and proof

Ordinary bounded objects and refs remain SQLite BLOBs. Multipart parts are
committed separately to SQLite; retries compare part digests and retained upload
identity. Completion freezes parts, streams one part at a time into a registered
private file, checks actual ciphertext SHA-256 and public framing/upload identity,
fsyncs the file and its directory, then publishes its immutable file pointer in a
SQLite transaction. Published files are never reopened for writing. The file
registry precedes creation so restart/abort/deletion can reclaim unreferenced
files. Incomplete idle uploads expire after seven days; admitted work is protected.

No atomic rename is used as a durability substitute: the full file and directory
are synchronized before the SQLite pointer commits. If a process dies before
publication, the retained parts allow retry and the registered unpublished file
is not visible. A committed pointer keeps the registered file from garbage
collection. Deletion unlinks and synchronizes the directory before retiring the
file registry row. SQLite free pages remain reusable; this is logical deletion,
not forensic secure erasure or automatic database-file compaction. Reserve space
for parts, assembled ciphertext and WAL during completion.

The revision-1 proof backend reads the actual admitted BLOB or one opened immutable
file and hashes shared-handler prefix + ciphertext. It counts bytes, rejects
truncation, and does not reuse cached upload hashes. Two proofs may run concurrently;
others receive 503. A 60-second deadline and HTTP disconnect cancellation bound the
read. Unit tests exercise corruption, truncation, missing object, challenge changes,
delete/read admission, cancellation and concurrency. Shared-handler HTTP vectors
and native large-object confirmation require the fixed protocol/SDK candidate;
backend tests alone are not that evidence.

## 中文交接摘要

当前使用单个 SQLite 数据库，WAL/FULL；完整对象读完后原子写入，共享协议引擎
等待依赖对象及整个聚合请求完成才提交 ref。成功 HTTP 响应在 SQL 提交之后。
截断上传不会发布部分对象；聚合上传可保留完整但未被引用的对象，重试可复用。
单请求 CAS 防止旧值覆盖竞争写入。服务不解密内容，独立对象路径的发布顺序仍
依赖合规 SDK，不能证明任意恶意客户端提交的引用可达性。

SIGKILL/容器重建只能证明进程崩溃恢复，不能代替断电、撕裂写入或设备缓存
故障测试。FULL 的保证依赖 OS、虚拟机、文件系统及设备正确执行同步请求。
这里没有把文件 rename 原子性当作断电持久性。

删除契约已冻结并实现：技术名称/URL永久保留，不会同名复用；显示标签可通过新
技术名称和新UUID重复使用。先持久化deleting墓碑，再拒绝新请求；已准入响应或
校验流继续持有租约。写入在异步读取后重新检查active，旧上传不能复活仓库。
清理分批执行，只有对象、分片和注册文件均清完才进入deleted；失败及重启恢复。

multipart分片持久保存，组装文件经完整摘要/身份校验、文件及目录同步后才发布
SQLite指针；未发布文件不可见，注册表支持崩溃后清理。空闲分片保留七天，活动
准入保护在途操作。需预留分片、组装文件及WAL磁盘空间；SQLite空页可复用，不
承诺取证级安全擦除或自动缩小数据库。

revision1校验读取真实持久对象，同一打开文件/SQLite副本在整个读取期间受删除
租约保护。最多两次并发，60秒截止，并传播HTTP取消。单测不代替共享HTTP协议
及新native的大对象端到端证据，正式发布前仍需联合验收。
