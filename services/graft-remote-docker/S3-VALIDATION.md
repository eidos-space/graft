# S3 candidate validation

Implementation worktree: `/Users/mayne/workspace/graft-docker-s3`, branch
`codex/docker-s3-20261103`, based on `4612498c`. The user authorized inclusion in
Docker PR #45 and the coordinated 0.1.0 release. Publication remains gated on
formal public-SDK acceptance; this document does not establish a published release.

## Behavior

- Default local storage retained. Explicit S3 configuration uses secret files,
  SigV4 and a pinned AWS SDK dependency lockfile.
- Immutable payloads and completed multipart objects use random keys under the
  configured bucket prefix. SQLite owns control metadata and atomic publication.
- Active-state rechecks prevent late uploads from publishing after a tombstone.
  Failed cleanup retains deleting state for retry. VersionId deletion reclaims
  owned versions rather than adding delete markers.
- Proof streams actual S3 content. Same-length corruption changes proof;
  truncation fails. Range mismatches and provider failures become structured errors.
- Storage mode/bucket identity is bound to the data directory. S3 schema version 5
  is rejected by older local-only binaries; local mode remains schema version 4.
- Local multipart parts and an assembled file are temporary disk requirements.
  SQLite can retain freed pages/WAL allocation for reuse. No automatic migration,
  multi-instance mode, or bucket-only restore is provided.

## Executed checks

- `pnpm test:docker`: **21 passed**, including five new focused S3/storage tests.
  Log `/tmp/graft-s3-unit-final.log`.
- Isolated real MinIO S3 integration: **passed**, 20,971,643-byte multipart object,
  persisted part restart/resume, S3 multipart publication, range spanning a part
  boundary, actual HTTP proof, same-length corruption, truncation rejection,
  tombstone admission and removal of all test object versions.
  Log `/tmp/graft-s3-minio-final.log`.
- Actual Docker S3 mode + native SDK 0.3.39: **passed**. Encrypted initialization,
  explicit restore, wrong-password rejection, independent clone, SQLite/attachment
  update, SIGKILL/recreation, backup-to-empty-volume restore, proof corruption and
  truncation, and deletion restart/idempotence/name reservation.
  Log `/tmp/graft-s3-docker-final.log`. Linux ARM64 image:
  `sha256:e390a8ca57cc585775e6d333fea9d81a4cb8088d31ccc08da86542be00875b64`.
  Native macOS SDK SHA-256:
  `3f1556acacd4bc0094d8be7da45503119359db787424939bcdd746b940fbc628`.
- Default local-mode Docker regression: **passed**; log
  `/tmp/graft-s3-local-regression.log`.
- Compose S3 configuration and workflow YAML validation, JavaScript syntax,
  formatting and `git diff --check`: passed. Production dependency audit reported
  zero vulnerabilities (`/tmp/graft-s3-npm-audit.json`).

MinIO fixture: local cached image
`quay.io/minio/minio@sha256:a640662d97632f7b94e9dee8cbb7da5c20db24879725cb4fac36f1e220cd528a`,
version RELEASE.2025-04-03T14-56-28Z. Tests create isolated buckets, generated test
credentials, containers and networks, and remove them afterward. No existing user
bucket, production data, or Cloudflare deployment was touched.

## Reproducible CI fixture and public-package gate

The cached upstream container cannot currently be pulled anonymously. CI builds
only the test fixture from official MinIO commit
`f2619d1f629db75c4282329352f4aaf79caba436` (tag
`RELEASE.2025-04-03T14-56-28Z`), with pinned Go and runtime base digests in
`test/Minio.Dockerfile`. It is not included in the Graft product image or release.
The local fixture image is
`sha256:a85f99418954512927bf309e1c95dc5bd5f3a3d7e29fbd2845ed3b4469d3a909`.
Its actual versioned-S3 integration passed; log
`/tmp/graft-s3-official-source-minio-final.log` (including strict stored-size checks).
The refreshed unit suite remains 21/21 (`/tmp/graft-s3-unit-release.log`).

The new S3 retained-challenge fault hook passed against the source SDK hash above:
`/tmp/graft-s3-retained-proof-source.log`, server image
`sha256:ab3cc47e712d7c2265bc290ed52d11b778889236cd9a2690b8589983dda3a88a`.
The 80 MiB lost-completion case rejected actual S3 ciphertext corruption,
preserved 83,906,816 spool bytes until success, and recovered with zero payload
readback (1,156 bounded header bytes, three proof requests). This remains source
SDK evidence, not a public npm SDK result.

`test/public-sdk.sh` runs the same server image against local disk and isolated S3,
using the published SDK/native pair. Both modes require an 80 MiB attachment and
80 MiB SQLite database, independent clone/explicit restore, ordinary upload and
response-loss recovery, process crash/reopen, multipart interruption, and lost
multipart completion plus a retained-proof challenge. The latter mutates actual
persisted ciphertext and requires rejection without publishing refs, preserving
the spool until the original bytes are restored and a fresh proof succeeds.
Linux AMD64 and ARM64 each run this gate before image publication. Test logs are
retained as workflow artifacts. Wiring this gate is not evidence that it passed.

## Limits

These results establish actual S3 API compatibility against MinIO, **not a real
Backblaze B2 cloud acceptance**. B2 endpoint/region/application-key examples follow
its official docs. No real B2 credentials were available or used. This candidate
has not yet completed public-SDK dual-mode CI or been published.

A process crash can orphan a provider-side multipart session; configure lifecycle
cleanup for incomplete uploads without expiring live objects. Keep both the local
control database and corresponding bucket data in coordinated backups. Object
Lock/retention or permission failures can keep deletion pending.
