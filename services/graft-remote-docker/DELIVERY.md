# Docker candidate delivery / Docker 候选交接

Authoritative worktree: `/Users/mayne/workspace/graft-docker-selfhost`
Branch: `codex/docker-selfhost-20261103`
Base: `3bf2647d1901d932a574e3e7b9518bacb54dae8c` (SDK 0.3.38 baseline).
This branch contains the Docker candidate; the release remains gated on the fixed SDK
and coordinated acceptance. The older shared directory contains the
first-stage implementation only; do not publish that dirty shared checkout.

## Release identity and gates

- Planned first version: `0.1.0`; image `ghcr.io/eidos-space/graft-remote-docker`.
- Tag: `graft-docker-v0.1.0`, independent of `v*`, `graft-sdk-v*`, `graft-remote-v*`.
- Publisher: `.github/workflows/docker-release.yml`, matching native Linux AMD64
  and ARM64 runners, GitHub token authentication, digest-pinned Compose and source
  commit/checksum assets in GitHub Release. It has not run remotely.
- No image/tag/release has been published. No production deployment performed.
- User authorized coordinated publication, **after** joint acceptance. The
  coordinator controls order; this task prepares Docker and does not publish early.
- A new Remote capability/package is now a dependency, in addition to the new SDK.
  Pin final published versions only after the proof protocol is frozen and tested.

## Shared management contract

Discovery: `management.version=1`, `capabilities=["rename","delete"]`, existing
`repositories` URL unchanged. DELETE name and GET name/status return
`{namespace,repository,status}`; deleting=202, deleted=200. Status may be active.
List includes active/deleting; deleted rows are omitted. Missing names return 404.
Technical name/URL never reused (PUT on tombstone=409); same display label can be
used under a new technical name and new encryption identity.

Tombstone is committed before acknowledgement. New protocol operations fail 410;
object-proof requests return 404 as required by the frozen proof contract.
Admitted readers keep a lease through response streaming/cancellation; cleanup
waits for leases. Writes recheck active state after body consumption. Deletion
resumes after restart, failures retain deleting, and physical cleanup is batched.

## Evidence retained

- Original 0.3.37 published SDK: full restore + independent clone, SQLite and
  attachment updates, Linux ARM64 container; source Lite CustomSyncClient tested.
  This remains **old-client** evidence, not new lightweight initialization evidence.
- Published SDK 0.3.37 also passed an 80 MiB random attachment plus 80 MiB SQLite
  BLOB database full-restore/independent-clone scenario, log
  `/tmp/graft-docker-large-db-037.log`. This remains old-client evidence.
- Current storage/management/proof suite: 16 cases passed, including schema migration and idle-upload expiry,
  including stale backend writes after tombstone, cleanup failure/restart, name
  reservation, atomic CAS, truncated aggregate publication, multipart digest and
  identity validation, interrupted/reopened multipart and streamed range reads.
- Initial 0.3.39 native SHA-256:
  `55773f7a40b2c1d3a84cd8e79afe623879f41d89db2de26ae0daea3cd609b441`.
  Path `/tmp/graft-sdk-0.3.39-candidate/graft-sdk.darwin-arm64.node`, matching
  macos-keychain release profile, JS in `/Users/mayne/workspace/graft-initial-confirmation/packages/graft-sdk`.
  SDK PR #44 candidate commit `98201cc809ce943cd92ce2c0ca178d6193e5d9c0`.
- **Failed 80 MiB case**: `/tmp/graft-docker-039-validation.log`. Default
  pushInitialBranch performed Range GETs of 72, 184, then **83,906,560 bytes** of
  actual ciphertext. This is not a bounded header probe. It blocks lightweight
  acceptance. The historical failing server image digest was not captured; later
  harness runs now record image ID/architecture explicitly.
- `/tmp/graft-docker-039-faults.log`: upload interruption and lost-object-response
  recovery passed against that old candidate. The ref-loss test did not inject
  because its matcher missed POST CAS; matcher is corrected. Remaining matrix
  must run with the replacement fixed SDK, not be inferred from those passes.
- No packaged Lite UI, published 0.3.39 SDK, native Linux AMD64 service run, or
  public GHCR pull has yet been verified. Cross-build is not runtime evidence.

## Current dependency / 当前依赖

Object proof contract **revision 1 is frozen**, in
`/Users/mayne/.codex/visualizations/2026/11/03/01a1a0ae-a786-7301-8e1c-cc0a53a4fe80/object-proof-contract.json`.
The Docker callback is implemented in `object-proof.mjs` and wired through the
repository admission lifetime in `app.mjs`. It hashes actual SQLite bytes or a
single open immutable file, never cached upload digests. Shared handler/types
remain owned by the SDK task and are not duplicated in this worktree.

Backend source-set SHA-256 (ordered path + NUL + content + NUL over root service
`.mjs`, Dockerfile, Dockerfile.dockerignore, compose.yaml, VERSION, SDK_VERSION):
`0dc6b79a1f9c6fd24780d2359db11d1fdb7130cf088e96bc36464da86539f66f` (working-tree snapshot, not a released commit).

Actual backend tests passed: challenge changes; same-size file corruption changes
proof; truncation rejects; absent object returns null; admitted proof completes
against the same file while deletion waits; already-aborted signal rejects;
concurrency saturation rejects with 503. The 60-second timeout is implemented but
**a real 60-second timeout scenario has not been exercised**. In-flight cancellation and actual HTTP shared-handler vectors passed.
HTTP evidence: `/tmp/graft-docker-proof-http-final.log`; source Remote 0.2.6,
commit `b26a222823b9e47f59c610de8d12f62c9838c410`, Linux ARM64 image
`sha256:271c7bd5ee3b8d00d8b6813a5129b8c183b07e86e6a5ef9b931e54146ec425b7`.
This fixture tests actual backend bytes, not native SDK cryptography. No O(n) 80 MiB proof
latency result exists yet. These unit tests do not establish end-to-end acceptance.

Next: source-candidate integration using `GRAFT_REMOTE_SOURCE` plus the fixed new
SDK/native hash, **without waiting for npm publication**. The old hash `55773f7a…`
is retained solely as failed multipart-readback history. After candidate joint
acceptance and coordinated Remote/SDK publication, run a separate public-package
and public-image verification. Do not confuse those two phases or create a
publication dependency cycle.

revision1已冻结，Docker后端已实现并通过上述单测；共享handler实际HTTP联测已通过；新native修复候选
到位后即可用源码联测80MiB，不等npm。超时实现不等于超时测试通过，单元通过
不等于HTTP端到端。联合候选通过后才按统筹顺序发布Remote/SDK，再做公开包与
公开镜像验收。旧55773f7a失败样本不得改写为成功。

## Commands after the fixed candidate arrives

```sh
cd /Users/mayne/workspace/graft-docker-selfhost
pnpm test:docker
GRAFT_TEST_SKIP_SDK=1 \
GRAFT_REMOTE_SOURCE=/absolute/fixed/packages/graft-remote \
GRAFT_CANDIDATE_SDK_PATH=/absolute/fixed/packages/graft-sdk \
GRAFT_CANDIDATE_NATIVE_PATH=/absolute/fixed/graft-sdk.darwin-arm64.node \
GRAFT_LARGE_PAYLOAD_MIB=80 pnpm test:docker:container
```

Keep each observed result. These are correctness scenarios, not repeated
performance samples. Final report must distinguish source/native/packaged/public
artifacts and process-crash evidence from actual power-loss durability.

## Final native candidate: environment failure retained

Source `1b2037e213240bd161517656e7d0921a7117c0e8`, native SHA-256
`fd3b996965f3263775988c3a47b4280caac4643d0a13237a5fe5313f0e042590`.
The 80 MiB attachment run reached explicit full restore after default publication,
restart and independent clone, then failed with host ENOSPC. The separate
80 MiB SQLite fixture failed while inserting random BLOBs because the disk was full.
Neither complete scenario is accepted. Logs retained under the durable evidence
directory as `graft-docker-proof-final-matrix.log` and
`graft-docker-proof-final-large-db.log`; retry must use separate log names.
Multipart recovery spool peak usage was not sampled; no total temporary-disk claim
is made. Successful default initialization does not imply zero upload spool usage.

PR #45 native Linux AMD64 and ARM64 container CI both passed on source
`b4ba05f2682efa6a8db156922f6463722b94c630`, run `41614829327`. Those jobs test
that branch's SDK/core baseline and do not replace final candidate proof acceptance.
The documentation link-check failure came from linking to not-yet-merged main
paths; fixed-commit links and local docs rebuild passed in follow-up `48ec43bb`.

## Final candidate functional retries after disk recovery

With the same `fd3b9969…` native, the complete six-scenario run exited 0:
`/tmp/graft-docker-proof-matrix-retry.log`. The normal case used an 80 MiB
attachment and multipart; the five fault cases used 2 MiB payloads targeting
ordinary object upload, object-response loss, ref-response loss, server SIGKILL
and client SIGKILL. Every injected scenario recovered to Connected, independently
cloned correct data and passed bidirectional updates. Normal initialization and
reopen/repeat confirmation downloaded 0 payload bytes, 0 header bytes and 8,855
metadata bytes total. The lost ordinary-object response case downloaded 215
bounded authenticated-header bytes and 0 payload bytes. In this log `fallback`
means aggregate capability suppression, not legacy object-proof fallback.
Image: `sha256:af5be99b0a8226b4d683649d4c68d2f5a7fef64cd05c65dc8c35995544c84707`.

The separate 80 MiB attachment + 80 MiB random SQLite BLOB scenario also exited 0:
`/tmp/graft-docker-proof-large-db-retry.log`. It made 16 multipart requests, with
0 initialization payload/header bytes and 9,528 metadata bytes including reopen
and repeated confirmation. Independent clone verified all database BLOB hashes;
explicit full restore and bidirectional updates passed. Image:
`sha256:3c8495a3186eab856479a619b88be1d5f9ce6adeb03c74c2214a3e2040fcec6e`.
Both runs also passed actual proof HTTP corruption/truncation/deletion checks and
delete-after-restart/name-reservation checks. Both used the actual Linux ARM64
Docker backend and macOS native SDK; neither is a public-package/image test.

The two targeted **80 MiB multipart faults** (part truncation and lost successful
complete response) are a separate pending acceptance step; the earlier 2 MiB
fault successes must not be used as evidence for them. Original ENOSPC logs remain.

## Multipart fault acceptance: one pass, one release blocker

`/tmp/graft-docker-proof-multipart-retry.log` retained a container restart readiness
failure before client recovery; the harness now waits for Docker's terminal state
before reconciling startup and captures state on readiness failure. No backend
implementation was changed to bypass this observation.

`/tmp/graft-docker-proof-multipart-retry2.log`, image
`sha256:a467140ad62d9714963a41e05041cb36a3ccc4db834662b6ce29f0e45bcc13f2`:

- 80 MiB part interruption **passed**. Exactly multipart part 1 was truncated;
  no initial ref/HEAD publication occurred. After reopen, successful challenge,
  Connected, 0 payload bytes, independent clone and bidirectional updates passed.
  Header probes: 450 bytes; successful proof response: 326 bytes.
- 80 MiB lost-complete recovery **failed acceptance**. The server completed with
  204, which the proxy discarded; it then generated a 200/326-byte proof response
  that was also discarded to keep the first attempt unconfirmed. After SDK close,
  server SIGKILL and clearing the network fault, the reopened SDK returned
  Connected but issued only 72+184-byte authenticated-header GETs, with **no new
  successful object-proof request** before ref/HEAD publication. The harness
  rejected this. It did not proceed to clone for this failed scenario.

The latter is not ENOSPC (3.3 GiB free after cleanup) and remains a release blocker
pending SDK-owner diagnosis/fix. Both original failures and successes are retained
in the durable evidence directory. Docker local large-data work is stopped so the
SDK owner can perform its focused investigation without competing disk pressure.

## Replacement candidate 889a: prior recovery blocker resolved

Superseding SDK source: `889a92462d4c8b6564c3da9da3962cc18b884bdb`.
Native `/tmp/graft-sdk-0.3.39-proof-889a/graft-sdk.darwin-arm64.node`, SHA-256
`3f1556acacd4bc0094d8be7da45503119359db787424939bcdd746b940fbc628`.
Old `fd3…` is not a releasable candidate. The new three-scenario run exited 0:
`/tmp/graft-docker-proof-889a-final.log`, actual ARM64 Docker image
`sha256:53101d74d7f1482e0582eab05f38f9e176c69f2b33491388646e75dc3da12511`.

- 80 MiB complete-response and first proof-response loss: first attempt failed
  without publishing refs/HEAD. Persisted ciphertext byte 256 was flipped while
  preserving its length. Reopened SDK rejected the resulting proof, did not write
  refs/HEAD, and retained the exact original 83,906,816-byte local ciphertext
  (same path, size and SHA-256). Restoring the byte and reopening required a new
  successful proof before Connected. The retained ciphertext was then removed.
  Payload readback: 0; bounded header probes: 1,156 bytes; proof requests: 3.
- 80 MiB multipart part-1 truncation: retained original ciphertext, successful
  proof on recovery, Connected, then spool removal. Payload readback: 0; bounded
  header probes: 706 bytes; proof requests: 1.
- 80 MiB normal publication/reopen: payload and header readback both 0; proof
  requests: 1. Explicit full restore passed.

All three independently cloned correct data and completed bidirectional updates.
Actual HTTP damage/truncation/deletion and delete-after-restart checks also passed.
Spool figures above are observed retained files at failure boundaries, not total
peak disk usage. Normal-case spool lifecycle was not separately sampled.
Same-hash large-SQLite and five small-object fault cases are being completed
separately before the final source-candidate handoff.

## Source-candidate acceptance complete (889a)

The same-hash large-SQLite + five small-object fault run exited 0:
`/tmp/graft-docker-proof-889a-database-faults.log`, ARM64 image `sha256:cdf9412e559c36c0c1bbbe70f500c39ff9988ebd623e8c7b5b65c5f0c607c50c`.
80 MiB attachment + 80 MiB SQLite: 16 multipart requests, two successful proof
responses (655 bytes), zero payload/header initialization readback, 9,528 total
metadata bytes including reopen/repeat confirmation. Independent clone verified
all BLOB content hashes; explicit full restore and bidirectional updates passed.
Ordinary upload interruption, lost object response, lost ref response, server
SIGKILL and client SIGKILL were each actually injected with 2 MiB payloads and all
recovered with correct clone and updates. Proof HTTP and deletion restart passed.

Docker's functional source-candidate acceptance is now complete for SDK commit
889a/native3f1556ac. This supersedes the fd3 blocker without rewriting that failure.
Pending release gates: final PR/CI checks, coordinator-approved Remote/SDK release
order, then separate public npm SDK and anonymous GHCR digest/Compose validation.
No public Docker release or production deployment has happened. Tests use actual
Linux ARM64 Docker and macOS SDK; new-candidate native Linux proof acceptance is
not inferred from baseline AMD64/ARM64 container CI. The first observed PR
performance run remains 41614829388; later correctness tests are not performance
resampling. Final mutable worktree test edits must be committed before release.

## Public Linux SDK gate (prepared, not executed)

After the six SDK 0.3.39 npm packages and Remote 0.2.6 are public, run
`Check Docker Graft server` with workflow-dispatch input `public_sdk=true` against
the final source commit. Both native ubuntu-24.04 AMD64 and ubuntu-24.04-arm jobs
install the public npm SDK and matching native package, then run 80 MiB
lost-complete/proof recovery (including real persisted corruption rejection and
spool checks), multipart truncation recovery, and normal/full-restore/clone/update
verification against the actual Docker backend. Each uploads its own proof log;
the harness records the installed native hash and server image ID. This job has
no package-write permission and publishes nothing.

The Docker release workflow independently repeats public-SDK verification on each
native architecture **before** pushing its image. Both architecture jobs must
succeed before the manifest and GitHub release are created. Baseline source SDK
CI does not satisfy the public-SDK proof gate. No dispatch has been requested yet.

## Published Remote 0.2.6 consumption

Merged main `91c15f85` (the formal Remote release source). Docker now defaults to
installing the actual public npm Remote 0.2.6 archive, pinned by REMOTE_VERSION and
SHA-256 `83b978c38d7d55fecb7519486d0063926ff3c16d8507ca9f456ba43cdc71830b`
in REMOTE_SHA256. The build fails on archive mismatch. Source candidate builds
remain explicit via GRAFT_REMOTE_SOURCE and are not the production default.

16 service tests passed after merging. The lightweight actual container run with
published Remote (no SDK or large-data work) exited 0, including real proof HTTP,
persisted corruption/truncation, deletion restart and backup/restore. Log:
`/tmp/graft-docker-public-remote026.log`; image `sha256:80f2ab1abff31a87ce56c2b3f7cebce6520108b1445af9c038ec14d42f759710`. This proves published
Remote consumption, not the still-pending public SDK Linux proof gate.

Historical a988df00 baseline CI: both native Docker jobs, docs and performance
passed; Windows transfer-progress cancellation test failed. Its failure log is
`/tmp/graft-docker-windows-ci.log`; new main contains the reviewed SDK/test changes
and the merged final head requires fresh CI rather than dismissing that failure.
