# Self-host Graft with Docker

[简体中文](README.zh-CN.md)

A personal Graft sync server for Eidos Lite and the native Graft SDK. It reuses
`packages/graft-remote` for Remote v1 and encrypted synchronization, and provides
Lite's discovery, repository creation, listing and display-name management API.
No Eidos account, billing service, Cloudflare account or external database is needed.

## Start

Requirements: Docker Engine with Compose v2. Run from a checkout of this repository:

```sh
cd services/graft-remote-docker
mkdir -p secrets
chmod 700 secrets
(umask 077; openssl rand -hex 32 > secrets/access-key)
# Compose mounts a host file; the unprivileged container must be able to read it.
# The containing host directory remains private. Do not commit this file.
chmod 444 secrets/access-key
# For local SDK testing only; Lite's normal UI requires HTTPS.
printf 'GRAFT_PUBLIC_ORIGIN=http://127.0.0.1:8787\n' > .env
docker compose up --build -d --wait
curl --fail http://127.0.0.1:8787/healthz
```

The image runs as UID/GID 1000, with a read-only root filesystem, dropped
capabilities and a persistent `graft_data` volume at `/data`. The default port
binding is loopback only. `restart: unless-stopped` restarts the process after
failure or host restart; a failed health check alone does not restart a container.
Do not run `docker compose down -v` against data you want to keep.

The access key grants full access to every repository on this server. Save it in
a password manager and enter it in Lite. It is separate from the repository's
**encryption password**, which is handled by clients and must never be sent to the
management API. The server reads the key from `/run/secrets/graft_access_key` at
startup; there is no public setup page, default credential or environment-token
fallback. To rotate it, replace `secrets/access-key` with another random key,
restore its permissions, run `docker compose up -d --force-recreate`, and update
all clients. Repository identities and encryption passwords remain unchanged.

## HTTPS and Eidos Lite

Use a dedicated hostname, for example `sync.example.com`, pointing at your server.
Set `.env` to:

```dotenv
GRAFT_PUBLIC_ORIGIN=https://sync.example.com
```

Run Caddy on the host with this Caddyfile (ports 80/443 must be reachable):

```caddyfile
sync.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

Caddy terminates TLS and obtains certificates. Recreate the Graft container after
changing `.env`. Keep port 8787 private. If the proxy runs in Docker, attach it to
the same Compose network and use `graft:8787` as its upstream instead; do not use
`127.0.0.1` from a separate proxy container. Configure your proxy to allow requests
up to 64 MiB and transfers lasting up to 120 seconds; preserve Authorization,
Graft-Protocol, Content-Length and protocol headers, with no path rewrites,
response transformations, caching, or redirects on API paths. Never log
Authorization headers or request bodies.

The service always uses `GRAFT_PUBLIC_ORIGIN` to generate discovery and repository
URLs, and ignores Host/Forwarded headers. TLS is enforced by your proxy; the
container itself serves HTTP on its private interface. Do not expose that interface
directly to an untrusted network. The origin cannot include a path, query or
credentials. HTTP origins are accepted only for loopback development.

In Lite, open the custom sync server settings, add `https://sync.example.com` and
the access key, then create or select a remote repository. Use Lite's encryption
password flow to connect. This is an independent remote: pointing an existing
connection at a different server does not migrate its encrypted identity or data.

## API and limits

| Method | Path | Authentication |
| --- | --- | --- |
| GET | `/`, `/healthz`, `/.well-known/graft` | Public |
| GET | `/api/graft/repositories` | Bearer key |
| PUT | `/api/graft/repositories/:name` | Bearer key |
| PATCH | `/api/graft/repositories/:name` | Bearer key |
| DELETE | `/api/graft/repositories/:name` | Bearer key |
| GET | `/api/graft/repositories/:name/status` | Bearer key |
| Protocol methods | `/personal/:name/...` | Bearer key and `Graft-Protocol: 1` |

PUT accepts `{"display_name":"Notes","encryption":"encrypted-remote-v1"}`;
an empty body defaults to encryption. Explicit `"plain"` is available for ordinary
SDK clients. PATCH accepts only `display_name`. Names are 1–64 ASCII letters,
digits, dots, underscores or hyphens, starting/ending with a letter or digit.
Display names are trimmed, NFC-normalized, 1–80 code points without controls.
Management JSON is limited to 4096 bytes. Unknown fields are rejected.

Creation is idempotent (201 new / 200 existing); an existing repository's profile
cannot change (409). Encryption UUIDs are persistent. Unknown repositories return
404 and are never implicitly provisioned. Responses match the personal Cloudflare
management contract, including `namespace: "personal"`, `remote_url`,
`created_at` in epoch milliseconds and the encryption descriptor. Listing returns
`{namespace, repositories}`. PATCH changes only the display label.

This is a **single-instance personal server**, not a multi-tenant service. SQLite
WAL with FULL synchronization stores the directory, identities and opaque objects
in one database. Object publication and compare-and-swap are atomic SQL statements;
unacknowledged requests can be retried after restart. Encrypted hint indexes are
rebuilt by the shared engine from persisted hints. Run one container per volume,
on local disk (not NFS), and monitor disk space. Repository deletion is explicit; no general reachability GC, storage quotas, account roles or replication are provided.

Individual HTTP requests are limited to 64 MiB; at most four HTTP requests are served at
once (503 with Retry-After when busy). The backend buffers individual objects and
uses synchronous SQLite, so large writes block other work. Compose limits memory
to 1 GiB. Encrypted and plain multipart uploads use 16 MiB parts and the protocol's maximum
of 10,000 parts. SQLite retains parts across restart. Completion streams them to a
private file, verifies the encrypted ciphertext identity/digest, synchronizes the
file and directory, then atomically publishes the SQLite metadata. Reads stream
large objects from disk. Space size is not limited to one HTTP request; disk space
and client protocol limits still apply. Consult validation evidence for actual
large-attachment coverage; theoretical part limits are not scale evidence.

## Backup, restore and upgrade

Keep the source revision and image identity with each backup. Back up the whole
`/data` directory while stopped; copying only the main SQLite file while running
can lose WAL data. From this directory:

```sh
mkdir -p backups
chmod 700 backups
docker compose stop graft
docker compose run --rm --no-deps -T --entrypoint tar graft -C /data -czf - . > backups/graft-data.tar.gz
chmod 600 backups/graft-data.tar.gz
docker compose start graft
```

Store backups off-host and protect them: repository labels and explicitly plain
repositories are not encrypted. Back up the access-key file separately and retain
the clients' encryption passwords; server backups cannot recover those passwords.
For restore, stop the target server and use an **empty** data volume. Preserve
UID 1000 ownership. With the target volume already created by Compose:

```sh
docker compose run --rm --no-deps -T --entrypoint tar graft -C /data -xzf - < backups/graft-data.tar.gz
docker compose up -d --wait
```

Verify listing, UUIDs and a client download before returning the server to use.
Never overlay a backup onto an active or nonempty database directory.

For upgrades, stop and back up first, review changes, check out the intended Graft
revision, then run `docker compose build --pull` and
`docker compose up -d --force-recreate --wait`. Retain the old image and backup
until verified. Schema version 4 is checked on startup; unknown versions fail
closed. A rollback that crosses a storage migration requires restoring the
matching pre-upgrade backup. No image is automatically pulled or published by this
repository. The Dockerfile installs the published Remote package pinned by
`REMOTE_VERSION` and verifies its npm archive against `REMOTE_SHA256`. The Node
base image is pinned to a reviewed digest. Explicit source-candidate tests use
`GRAFT_REMOTE_SOURCE`; production builds use the published package by default.

## Verification

From the repository root, with pnpm dependencies installed:

```sh
pnpm test:docker
# Requires Docker + an E2EE-capable built Node SDK at packages/graft-sdk:
pnpm test:docker:container
# Build the E2EE SDK explicitly if your existing binary uses the plain profile:
bash packages/graft-sdk/scripts/build-native.sh "$(rustc -vV | sed -n 's/^host: //p')" packages/graft-sdk/native e2ee
# Optional: point at another built SDK package
GRAFT_SDK_PATH=/absolute/path/to/sdk pnpm test:docker:container
```

The container script creates a uniquely named Compose project, temporary access
key and volume, then removes that test data in `finally`. It checks actual image
build/start, health, unauthorized rejection, canonical discovery URLs, ordinary
uploads, SIGKILL recovery, container recreation, stopped backup/empty-volume restore, and native SDK encrypted
initialization/push, wrong-password rejection, post-crash clone, SQLite and
attachment updates in both directions. It never targets a production server.
`GRAFT_TEST_SKIP_SDK=1` runs only container/protocol checks and is not E2EE evidence.

A green health check proves process/database readability; the SDK test proves
native protocol interoperability. Neither by itself proves the packaged Lite UI
or an operator's public HTTPS configuration. See [validation evidence](VALIDATION.md).

To exercise an available Lite source checkout without opening a user profile, set
`GRAFT_LITE_CLIENT_PATH` to its `src/main/sync/custom-sync-client.ts` and install Bun.
The container test invokes that actual class for discovery, invalid-key rejection,
creation, descriptor validation, rename and listing. This remains a source-client
test, not packaged Electron UI verification.

Storage acknowledgement/order guarantees and the distinction between process
recovery and power-loss safety are documented in [the storage review](STORAGE-REVIEW.md).

## Rename and deletion

Discovery advertises `management.version: 1` and `capabilities: ["rename", "delete"]`.
DELETE returns `{namespace, repository, status}` with 202 while `deleting` and 200
when `deleted`; GET `.../:name/status` exposes the same state. Repeated deletion is
idempotent. Listing includes active/deleting repositories and omits completed
ones. New protocol requests after deletion starts receive 410. Accepted streams
are drained/cancelled before physical cleanup. Cleanup runs in bounded batches,
resumes at startup, and remains `deleting` after an I/O failure.

**Technical names and URLs are never reused.** PUT on a deleted name returns 409;
create a new technical name to reuse a display label. This preserves identity
isolation and prevents stale uploads/delete retries from targeting a replacement.
Deletion is irreversible and removes server copies, not local device files.

## GitHub release deployment (pending first release)

The planned image is `ghcr.io/eidos-space/graft-remote-docker`, with independent
`graft-docker-vX.Y.Z` tags. Download `compose.yaml`, `SHA256SUMS`, `image.json` and
`SOURCE_COMMIT` from that GitHub Release after it is published. Its Compose asset
pins the immutable image digest. Place it in a private directory, create the
`secrets/access-key` and `.env` as above, then use `docker compose pull` and
`docker compose up -d --wait` (no source build). For upgrades, stop/back up first,
keep the old Compose file, download and verify the new release assets, and pull
and recreate. Do not replace a digest with `latest`.

Publication requires both native Linux architecture jobs, the fixed public SDK
from `SDK_VERSION`, and the merged release PR. Local source-client or macOS addon
tests do not replace those gates. The first GHCR package must be publicly readable;
an anonymous pull is required before release acceptance.

## Candidate integration tests

To test an unpublished SDK separately from the old full-restore path:

```sh
GRAFT_TEST_SKIP_SDK=1 \
GRAFT_CANDIDATE_SDK_PATH=/absolute/candidate/packages/graft-sdk \
GRAFT_CANDIDATE_NATIVE_PATH=/absolute/candidate/graft-sdk.darwin-arm64.node \
GRAFT_LARGE_PAYLOAD_MIB=80 pnpm test:docker:container
```

The test records native SHA-256, image ID/architecture, successful payload reads,
404 probes and byte totals. It checks default publication confirmation, explicit
full restore, independent cloning and failure recovery. `GRAFT_CANDIDATE_SCENARIOS`
can select a comma-separated subset for debugging; it is not a full matrix pass.
`GRAFT_TEST_IMAGE` tests an already-built/pulled image without rebuilding it.

Incomplete multipart uploads expire after seven idle days; admitted work is protected.
Allow temporary disk space for retained parts, assembled files and SQLite WAL.
Deletion is logical removal, not a guarantee of forensic erasure or automatic
SQLite file compaction. The object-proof backend reads real persisted ciphertext,
limits proofs to two concurrent reads and a 60-second deadline, and honors HTTP
cancellation. Its new capability is advertised only once the shared Remote 0.2.6
handler is integrated; backend-only tests do not establish native end-to-end proof.
