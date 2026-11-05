# Browser setup validation

Candidate branch: `codex/docker-web-setup-20261105`, based on `fa1f6b05`.
Validated on 2026-11-05. This is local candidate evidence, not a published image.

## Behavior

- Default Compose starts without a key file. `/setup` is bilingual; local storage
  needs no fields, S3 credentials and connection checks are in the browser.
- The operator obtains a one-time initialization link using `manage.mjs setup-link`.
  Its secret is in the URL fragment, removed immediately by the page, and never
  appears in ordinary server logs. Completed setup cannot be replayed.
- Configuration and credentials are private files in the persistent data volume.
  File contents and parent directory are synchronized before activating storage.
- Existing external-key deployments continue to work. Missing configuration on
  an existing database fails closed. Web-managed and external configurations
  cannot silently override each other.
- `/livez` is container liveness; `/healthz` remains unavailable until initialized.
- Caddy is optional, in its own Compose overlay. Public TLS is supplied by the
  operator's existing proxy/platform or the optional Caddy service.

## Executed checks

- `pnpm test:docker`: 27 tests passed, including initialization, origin checks
  (including `Origin: null`), authentication, bounded request bodies, replay,
  concurrent submissions, restart persistence and legacy compatibility.
- Built the actual Linux ARM64 image using the pinned Node base and published
  Remote 0.2.6 package. Candidate image ID:
  `sha256:411267141ac7d32bcf32ff7b35caf8a785d71ddacd3957bb9f8a0e661c21b872`.
- `test/setup-container.mjs`: local and real MinIO S3 passed initialization,
  S3 wrong-credential retry, read/write/delete probing, container recreation,
  authenticated repository access, explicit key recovery and secret-log checks.
- The same setup test ran public SDK 0.3.39 against both storage modes: native
  E2EE initialization, verified first push, wrong-password rejection, independent
  device clone, SQLite and attachment round trips passed.
- Existing `test/container.mjs` with the external-key overlay passed protocol,
  SIGKILL recovery, recreation, stopped backup/empty-volume restore, HTTP proof
  and permanent deletion/name reservation. This run skipped SDK; SDK evidence
  comes from the separate setup test above.
- Real browser on the release Compose template: English and Chinese setup,
  local completion, S3 field disclosure, switching from an invalid S3 field back
  to local, secret-free refresh after completion and 390px layout passed.
- Optional Caddy Compose configuration resolves successfully with the pinned
  multi-architecture Caddy digest. No public certificate issuance was attempted.

## Reproduce

```sh
npm ci --prefix services/graft-remote-docker --ignore-scripts
pnpm test:docker
docker build -t graft-web-setup:dev -f services/graft-remote-docker/Dockerfile .
GRAFT_TEST_IMAGE=graft-web-setup:dev \
GRAFT_S3_TEST_IMAGE=graft-minio-fixture:2025-04-03 \
GRAFT_SDK_PATH=/absolute/path/to/public-sdk \
pnpm test:docker:setup
```

Build the S3 test image with `test/Minio.Dockerfile` if unavailable. The release
publisher runs setup/container/SDK checks on both native Linux architectures.
Those publisher runs, public image verification and real public HTTPS acceptance
remain release gates; local ARM64 evidence does not substitute for them.
