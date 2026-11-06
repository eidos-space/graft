# Self-host Graft with Docker

[简体中文](README.zh-CN.md)

A personal Graft sync server for Eidos Lite and the native Graft SDK. It reuses
`packages/graft-remote` for Remote v1 and encrypted synchronization, and provides
Lite's discovery, repository creation, listing and display-name management API.
No Eidos account, billing service, Cloudflare account or external database is needed.

## Start

Requirements: Docker Engine with Compose v2. Run from a checkout of this repository:

**Get the initialization link before opening the browser.** Run the commands on
your server, from the directory containing the Compose file. The server root
address and `/setup` alone do not authorize setup or storage configuration.

```sh
cd services/graft-remote-docker
# For local SDK testing only; Lite's normal UI requires HTTPS.
printf 'GRAFT_PUBLIC_ORIGIN=http://127.0.0.1:8787\n' > .env
docker compose up --build -d --wait
docker compose exec graft node services/graft-remote-docker/manage.mjs setup-link
```

Open the **complete printed URL, including `#token=…`**, in your browser. The
server verifies the link before showing storage fields. Choose **Local disk** (no
additional fields) or **S3-compatible storage**. For S3, enter your existing private
bucket's endpoint, region and credentials, then test the connection. The test
writes, reads and deletes a temporary object; it does not verify large multipart
uploads. Advanced settings include a dedicated prefix and addressing style.

Click **Complete setup**. Save the displayed server address and access key, then
use them in Lite. The page supports English and Simplified Chinese. No manual key
generation, secret file or file-permission commands are needed.

The initialization link proves access to the server terminal. Keep it private.
It survives restarts until setup completes, then becomes invalid. It is never
printed in normal server logs. Opening `/setup` without it cannot claim a server.
Before setup, `/healthz` returns 503 and synchronization is unavailable; `/livez`
reports that the process is running and is used by the container health check.
After setup, check `/healthz` and try synchronization from a second client.

If **Test connection** or **Complete setup** stays disabled, first run the
`setup-link` command above and open its complete output. Filling storage fields
cannot supply the missing authorization. For an invalid or incomplete link,
retrieve it again from the same installation. Links do not expire with time;
they stop working when setup completes. If setup is already complete, use your
saved key or the `access-key` command below. If verification cannot connect,
check HTTPS and retry; open the link directly at `GRAFT_PUBLIC_ORIGIN` without
a redirect to another origin.

The image runs as UID/GID 1000, with a read-only root filesystem, dropped
capabilities and a persistent `graft_data` volume at `/data`. The default port
binding is loopback only. `restart: unless-stopped` restarts the process after
failure or host restart; a failed health check alone does not restart a container.
Do not run `docker compose down -v` against data you want to keep.

The access key grants full access to every repository on this server. Save it in
a password manager and enter it in Lite. It is separate from the repository's
**encryption password**, which is handled by clients and must never be sent to the
management API. Web setup stores the access key and optional S3 credentials in a
private configuration file in `/data`. Retain this volume during upgrades and
protect its backups. Restarting does not reset setup. If you lose the key, retrieve
it explicitly from the server terminal:

```sh
docker compose exec graft node services/graft-remote-docker/manage.mjs access-key
```

### Existing and automated deployments

Existing Compose files that mount `/run/secrets/graft_access_key` continue to work
and bypass web setup. Keep those files when upgrading. With the new templates,
use `compose.secrets.yaml` to supply `secrets/access-key`; add `compose.s3.yaml`
only for externally configured S3 storage. For example:

```dotenv
COMPOSE_FILE=compose.yaml:compose.secrets.yaml:compose.s3.yaml
```

Do not add these overlays to a web-configured installation. Changing storage
requires a separate server and client migration, not rerunning setup. An existing
database without its original key/configuration refuses to reopen setup.

## HTTPS and Eidos Lite

Use a dedicated hostname, for example `sync.example.com`, pointing at your server.
Set `.env` to:

```dotenv
GRAFT_PUBLIC_ORIGIN=https://sync.example.com
```

**Caddy is optional.** Use an existing HTTPS proxy or your platform's HTTPS
endpoint. Forward to `127.0.0.1:8787` on the host, or `graft:8787` from a container
on the Compose network. Complete setup at the configured HTTPS origin.

If you need a proxy, use the supplied `compose.caddy.yaml` and `Caddyfile`.
Point your hostname at the server and allow ports 80/443, then add to `.env`:

```dotenv
GRAFT_DOMAIN=sync.example.com
COMPOSE_FILE=compose.yaml:compose.caddy.yaml
```

Run `docker compose up -d --wait` (add `--build` when building from source).
Caddy manages certificates and redirects. Keep its data volume for certificate
renewal. Do not enable this overlay if another service already owns ports 80/443.
Alternatively, an existing host Caddy can use:

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
| GET | `/setup`, `/livez` | Public; contains no credentials |
| POST | `/setup/status` | One-time setup credential and same Origin; verifies permission without configuring storage |
| POST | `/setup`, `/setup/test` | One-time setup credential and same-origin JSON; only before setup |
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
WAL with FULL synchronization in default local mode stores the directory, identities and opaque objects
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
repositories are not encrypted. Web configuration includes credentials in `/data`.
For external-key installations, back up the access-key file separately. Retain
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
until verified. Schema version 4 (local) or 5 (S3) is checked on startup; unknown versions fail
closed. A rollback that crosses a storage migration requires restoring the
matching pre-upgrade backup. No image is automatically pulled or published by this
repository. The Dockerfile installs the published Remote package pinned by
`REMOTE_VERSION` and verifies its npm archive against `REMOTE_SHA256`. The Node
base image is pinned to a reviewed digest. Explicit source-candidate tests use
`GRAFT_REMOTE_SOURCE`; production builds use the published package by default.

## Verification

From the repository root, with pnpm dependencies installed:

```sh
npm ci --prefix services/graft-remote-docker --ignore-scripts
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

## GitHub release deployment

The image is `ghcr.io/eidos-space/graft-remote-docker`, with independent
`graft-docker-vX.Y.Z` tags. Download `compose.yaml`, `SHA256SUMS`, `image.json` and
`SOURCE_COMMIT` from that GitHub Release after it is published. Its Compose asset
pins the immutable image digest. Place it in a private directory, create
`.env` with your public origin, then use `docker compose pull` and
`docker compose up -d --wait` (no source build). For upgrades, stop/back up first,
keep the old Compose file, download and verify the new release assets, and pull
and recreate. Do not replace a digest with `latest`. Follow the instructions
included with that release: the 0.1.1 image predates web setup and requires an
external access-key file. Web setup is available in 0.2.0 and later.

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

## S3-compatible storage and automated configuration

For interactive installation, select S3 in `/setup`; no S3 overlay is needed.
For automated installations, use `compose.secrets.yaml` and `compose.s3.yaml`.
This mode stores immutable objects and completed large
ciphertext in the bucket. SQLite retains repository identities, refs/config,
object pointers, deletion state and unfinished upload parts. `/data` is still
required: budget temporary disk for upload parts and one assembled object per
active completion. The assembled local file is removed after upload and the temporary part rows
are deleted. SQLite can retain allocated free pages/WAL space for reuse; the data
volume does not necessarily shrink immediately. This is not a stateless service
or a direct-to-bucket client upload.

1. Create a dedicated bucket or exclusive prefix. For B2, create a **standard
   application key**, limited to that bucket, with read/write/delete access.
   Use the bucket's S3 endpoint and matching region. See the official
   [B2 JavaScript SDK configuration](https://www.backblaze.com/docs/cloud-storage-use-the-aws-sdk-for-javascript-v3-with-backblaze-b2)
   and [application key guide](https://www.backblaze.com/docs/cloud-storage-application-keys).
2. Copy `.env.s3.example` (release asset: `env.s3.example`) to `.env` and replace the sample origin, endpoint, region
   and bucket. Supply an externally managed Graft access key in `secrets/access-key`.
   For a new automated installation only, create it with:

   ```sh
   mkdir -m 700 secrets
   (umask 077; openssl rand -hex 32 > secrets/access-key)
   chmod 444 secrets/access-key
   ```
3. Save the B2 `keyID` in `secrets/s3-access-key-id` and `applicationKey` in
   `secrets/s3-secret-access-key`. Use the same protected directory/readable-file
   permissions as `secrets/access-key` (`secrets` 0700; mounted files 0444).
   Credentials are supplied as secret files, never stored in SQLite or remote URLs.
4. Start **with a fresh data volume**:

   ```sh
   docker compose -p graft-s3 -f compose.yaml -f compose.secrets.yaml -f compose.s3.yaml up --build -d --wait
   ```

For a GitHub Release deployment, download its `compose.yaml`, `compose.secrets.yaml`, `compose.s3.yaml`
and `env.s3.example` assets and verify `SHA256SUMS`. That `compose.yaml` already
pins the released image digest; use the same command above without `--build`.
When using the source checkout's `compose.release.yaml` template instead, set
`GRAFT_IMAGE` to the released immutable digest.

Keep `-p graft-s3 -f compose.yaml -f compose.secrets.yaml -f compose.s3.yaml` on all subsequent Compose
commands, including stop/start, backup, restore and upgrades. Omitting the project
or overlay can select a different volume or start the wrong storage mode.

| Setting | Purpose |
| --- | --- |
| `GRAFT_STORAGE` | `local` (default) or `s3` |
| `GRAFT_S3_ENDPOINT` | HTTPS origin; B2 example `https://s3.us-west-004.backblazeb2.com` |
| `GRAFT_S3_REGION` | Region matching the endpoint |
| `GRAFT_S3_BUCKET` | Existing bucket; server does not create it |
| `GRAFT_S3_PREFIX` | Exclusive nonempty prefix ending in `/`; default `graft/` |
| `GRAFT_S3_FORCE_PATH_STYLE` | `true` (default) or `false` |
| `GRAFT_S3_ACCESS_KEY_ID_FILE` / `GRAFT_S3_SECRET_ACCESS_KEY_FILE` | Mounted credential files |
| `GRAFT_S3_SESSION_TOKEN_FILE` | Optional mounted session token for temporary AWS credentials |

For Cloudflare R2, use a token restricted to the intended bucket with **Object
Read & Write** access and these values (no Worker is required):

```dotenv
GRAFT_S3_ENDPOINT=https://<account-id>.r2.cloudflarestorage.com
GRAFT_S3_REGION=auto
GRAFT_S3_BUCKET=your-dedicated-bucket
GRAFT_S3_PREFIX=graft/
```

R2 uses the same secret files and Compose overlay. Its S3 API does not implement
bucket versioning, so deletion removes the current object directly. Versioned
S3 providers instead use VersionId deletion. R2 aborts incomplete multipart
uploads after seven days by default. See [R2 S3 API compatibility](https://developers.cloudflare.com/r2/api/s3/api/)
and [multipart upload lifecycle](https://developers.cloudflare.com/r2/objects/upload-objects/).

The service uses SigV4, Get/Head/Put/DeleteObject, Create/Upload/Complete/Abort
MultipartUpload, and HeadBucket. Grant read/write/delete (including object-version
delete if versioning is enabled); HeadBucket requires bucket access. It does not
need account-wide bucket listing or bucket creation permission. It verifies the
bucket connection at startup; a provider outage never silently falls back to disk.
HTTP endpoints are rejected unless `GRAFT_S3_ALLOW_HTTP=true`, intended only for
isolated local test services.

An uploaded random object key is published in SQLite only after S3 acknowledges
completion. Refs remain local transactional updates. Challenge proofs stream the
actual S3 object, not a stored upload digest. Deletion waits for admitted readers
and removes tracked remote objects; errors leave the repository `deleting` for
retry. Versioned objects are deleted by VersionId so owned versions are reclaimed,
subject to bucket retention/Object Lock permissions. Do not modify this prefix
externally or add delete markers. Configure your provider to expire abandoned S3
multipart uploads: a process crash can leave a provider-side multipart session
before its ID is durably recoverable by the SDK. Never expire live object keys.

**Backup and restore require both `/data` and the matching bucket objects.** Stop
writes while taking a coordinated backup. A bucket alone cannot reconstruct refs
or repositories. Restoring only an older SQLite backup after deleting bucket data
cannot restore those objects. Run only one server for a data directory; sharing a
bucket does not make multiple independent SQLite databases a clustered service.

The stored backend identity prevents accidentally changing mode, endpoint, region,
bucket or prefix on an existing data directory. Automatic local-to-S3 or S3-to-S3
migration is not implemented. Keep the original service and volume intact; create
a fresh installation and synchronize data into it through a client if migrating.
Changing credential files for the same backend is supported after restart.

Verification: `npm ci --prefix services/graft-remote-docker --ignore-scripts`, then
`pnpm test:docker`. An explicit isolated MinIO image can run
`GRAFT_S3_TEST_IMAGE=<image> node --test services/graft-remote-docker/test/s3.integration.mjs`.
The full Docker harness accepts `GRAFT_TEST_S3=1 GRAFT_S3_TEST_IMAGE=<image>` and
creates its own bucket and random credentials; it never targets an existing user
bucket. MinIO success is S3 compatibility evidence, **not a verified B2 deployment**.
