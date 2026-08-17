# Graft Remote Sync 1.0

Status: Draft implementation-aligned normative baseline
Version: 1.0
Published: 2026-08-11
Canonical language: English

## Abstract

This specification defines Graft remote configuration, backend semantics,
version 1 HTTP transport, immutable object publication, ref compare-and-swap,
refspecs, and repository `fetch`, `push`, `pull`, and `clone`. Its purpose is to
make remote synchronization safe across the filesystem, S3-compatible, HTTP,
native SDK, and browser-capable implementations.

## 1. Scope and layering

Remote synchronization transports three distinct classes of state:

```text
repository objects/packs          immutable
SQLite commits/segments + files  immutable
HEAD and refs/**                  mutable transactional metadata
```

Repository identity and reachability are defined by [Graft Repository
1.0](./graft-repository-1.0.md). Snapshot content is defined by [Graft Storage
and Snapshots 1.0](./graft-storage-snapshots-1.0.md). Pull's local integration
uses [Graft Merge 1.0](./graft-merge-1.0.md).

## 2. Remote configuration and URI grammar

Named remotes are stored as structured repository configuration. Supported
backend forms are:

| URI/config | Meaning |
| --- | --- |
| `memory` | process-local test/temporary backend |
| `fs:///absolute/path` | filesystem or mounted-object backend |
| `s3://bucket/prefix` | S3-style backend |
| `s3_compatible://bucket/prefix?endpoint=https://...` | custom S3-compatible service |
| `https://host/<repository-path>` | canonical Graft HTTP remote |
| `graft+https://host/<repository-path>` | explicit HTTPS transport alias |
| `graft+http://host/<repository-path>` | trusted/local HTTP development |

Filesystem roots MUST be absolute. S3 configuration stores bucket, optional
prefix, and optional endpoint, but MUST NOT store access keys or secrets.

For HTTP, the configured repository path is the opaque protocol base. Clients
remove a `graft+` transport prefix and one trailing slash before requests. A
CLI-only `token_env` query MAY select the environment variable from which a
token is read; it is removed from the wire URL. Version 1 rejects URL userinfo,
fragments, and any other query parameter. SDK profiles use explicit in-memory
credentials and reject URL-carried credential selectors.

Remote names, branch names, and refspec destinations MUST pass repository ref
validation. Configuring a remote is local-only and does not contact it unless
the adapter explicitly offers validation.

## 3. Credentials and transport policy

HTTP authentication uses an optional bearer token:

```http
Authorization: Bearer <token>
```

CLI/environment policy may default to `GRAFT_REMOTE_TOKEN` or a configured
`token_env`. Rust/Node SDK credentials are explicit per-session/per-call
in-memory maps. Credentials MUST NOT appear in repository config, persisted
URLs, status caches, logs, errors, request IDs, or returned JSON. Implementations
SHOULD zeroize native secret buffers when no longer required.

Production HTTP remotes SHOULD use HTTPS. The current native clients use
dedicated HTTP/1.1 pools for ordinary reads, existence probes, and mutations,
with a 5-second connect timeout and a 30-second protocol request timeout. The
S3-compatible backend adds its storage-layer retry policy. Exact retry counts
outside the multipart rules are adapter policy, while the publication-safety
rules in Section 11 are normative.

## 4. HTTP protocol version and repository descriptor

Every protocol request carries:

```http
Graft-Protocol: 1
```

Every protocol response, including errors, MUST echo its supported version.
Unsupported versions SHOULD return `426 Upgrade Required`.

`GET {base}` returns a `graft-remote` descriptor with version, capabilities,
and optional limits. Version 1 capabilities include, when implemented:

```text
range
list
list-cursor
put-if-absent
read-bundle
fetch-bundle
upload-bundle
receive-pack
receive-bundle
multipart-object
cas
cad
```

Clients MUST ignore unknown descriptor fields/capabilities and MUST use the
documented fallback when an optional aggregate operation is absent.

## 5. Object keys and backend contract

Known keys include:

```text
HEAD
refs/heads/<branch>
objects/<fanout>/<object-id>
objects/pack/<pack-id>.pack
objects/pack/<pack-id>.idx
store/files/<fanout>/<content-id>
logs/<log-id>/commits/<lsn>
segments/<segment-id>
```

`HEAD` and `refs/**` are transactional. All other exposed version 1 repository
data is immutable after creation. `locks/**` is reserved and MUST NOT be
exposed as repository data.

Each key segment is independently percent-encoded UTF-8. Empty, `.`, `..`,
backslash, slash-as-data, NUL/control, invalid encoding, and reserved-lock paths
MUST be rejected. The current service library limits an object path to 768
UTF-8 bytes and mutable comparison metadata to 16 KiB.

An `objects/pack/<pack-id>.idx` version 1 document MAY carry an additive
`commits` array beside its authoritative object offset entries. Each item gives
a commit object ID contained in that pack and its parent object IDs. A client
MAY use this graph only to select immutable packs for speculative prefetch. It
MUST still decode every used object and validate its content-addressed ID;
missing, incomplete, or incorrect ancestry MUST fall back to ordinary verified
object discovery and MUST NOT change fetch results. Writers SHOULD include the
hint for every commit object added by that publication. Readers MUST accept
version 1 indexes that omit it.

A backend implements byte-preserving `head`, ranged/full `get`, transactional
`put/delete`, create-only `putIfAbsent`, `compareAndSwap`, `compareAndDelete`,
sorted recursive `list`, and optionally multipart upload. Repository isolation
MUST prevent one logical repository from addressing another backend namespace.

## 6. Version 1 operations

All routes are relative to the repository base:

| Request | Purpose | Success |
| --- | --- | --- |
| `HEAD /raw/<key>` | existence and full byte size | `200` |
| `GET /raw/<key>` | full or single-range read | `200`/`206` |
| `PUT /raw/<key>` | replace transactional metadata | `204` |
| `DELETE /raw/<key>` | delete transactional metadata | `204` |
| `PUT /raw-if-not-exists/<key>` | create immutable object | `204` |
| `GET /list?prefix=...` | sorted recursive key listing | `200` |
| `POST /read-bundle` | explicit immutable object batch read | `200` |
| `POST /fetch-bundle/<ref-key>` | bounded reachable pack stream for fetch | `200` |
| `POST /cas/<key>` | compare and replace metadata | `204` |
| `POST /cad/<key>` | compare and delete metadata | `204` |
| `POST /upload-bundle/<ref-key>` | stable clone snapshot stream | `200` |
| `POST /receive-pack/<ref-key>` | pack/index then ref CAS | `204` |
| `POST /receive-bundle/<ref-key>` | immutable dependencies, pack/index, ref CAS | `204` |
| multipart start/part/complete/abort | resumable immutable object transfer | `200`/`204` |

`HEAD /raw` is mandatory. A client MAY use the documented `GET` with
`Range: bytes=0-0` compatibility probe only after explicit `405` or `501`.
Authentication, transport, and other errors MUST NOT trigger that fallback.

Single byte ranges use standard `Range`/`Content-Range`; multiple or invalid
ranges return `416`. Large bodies SHOULD stream rather than buffer.

Listing is recursive, bytewise-lexically sorted, and cursor-paginated. A cursor
is opaque and tied to its prefix. Every matching key appears exactly once in a
stable traversal. The current service defaults to 100 keys per page and accepts
at most 500.

## 7. Atomic create and compare operations

`raw-if-not-exists` MUST atomically create or report an existing key. Existing
immutable content is never unconditionally overwritten. Clients that need
idempotence either trust a content-addressed key or read and verify a collision.

CAS/CAD expected state is encoded by:

```http
x-graft-expected-present: true|false
x-graft-expected-hex: <lowercase exact bytes, or empty>
```

Absent differs from a present zero-byte value. Comparison and mutation are one
linearizable operation per transactional key. A mismatch returns `409` and
does not mutate the key. CAS/CAD are REQUIRED for branch refs; unconditional
metadata writes are not a safe push publication primitive.

## 8. Aggregate and multipart transport

### 8.1 Read bundle

`read-bundle` collapses a finite set of immutable object reads into one
authenticated request. The request is UTF-8 JSON with one through 256 unique,
valid immutable paths:

```json
{ "version": 1, "paths": ["logs/example/commits/0000000000000001"] }
```

The service sorts paths bytewise and returns one frame per path using the same
`(path length, object length, path, bytes)` network-byte-order framing as
`upload-bundle`. The response uses
`Content-Type: application/vnd.graft.read-bundle`, declares the exact frame
count in `x-graft-bundle-objects`, and declares the exact complete length in
`x-graft-bundle-total-bytes` and `Content-Length`. Missing objects fail the
aggregate request. The current service caps the complete response at 64 MiB.
Clients MUST validate unique expected paths, lengths, final framing, and object
contents before use. They fall back to bounded individual reads on `404`,
`405`, or `413`.

### 8.2 Fetch bundle

`fetch-bundle` collapses ref discovery, pack-index discovery, and reachable pack
reads into one authenticated request. The request is UTF-8 JSON containing the
client's last fetched commit, or `null` when it has none:

```json
{ "version": 1, "have": "<64-byte lowercase object ID or null>" }
```

The service reads the requested ref and uses pack-index `commits` ancestry only
as a bounded selection hint. It streams the selected `.idx` and `.pack` objects
with the same manifest and frame format as `upload-bundle`, then confirms that
the ref did not change. The response uses
`Content-Type: application/vnd.graft.fetch-bundle`. The current service caps a
response at 128 packs and 48 MiB.

Clients MUST decode and content-hash every imported object, MUST complete
ordinary verified graph discovery after import, and MUST NOT treat the pack
ancestry hint as repository truth. They fall back to ordinary fetch on `404`,
`405`, `409`, `413`, or `422`; authentication, transport, malformed framing,
and object-integrity errors MUST NOT silently fall back.

### 8.3 Upload bundle

`upload-bundle` reads the requested ref, enumerates immutable keys, reads the
ref again, and returns `409` if it changed. A stable response contains a
length-delimited UTF-8 manifest followed by strictly sorted unique binary
frames `(path length, object length, path, bytes)` in network byte order.

The response uses
`Content-Type: application/vnd.graft.upload-bundle` and
`x-graft-bundle-manifest-bytes`. It MUST also declare the exact complete framed
body length in `x-graft-bundle-total-bytes`; `Content-Length` SHOULD carry the
same value when the host permits an explicit length on a streaming response.
Clients MUST prefer the Graft total-length header for transfer progress, MAY
fall back to `Content-Length` for older services, and MUST reject the response
when both are present but disagree. Its manifest is exactly:

```json
{
  "version": 1,
  "reference": {
    "path": "refs/heads/main",
    "value_hex": "<lowercase exact ref bytes>"
  },
  "objects": 3
}
```

Each following frame is a 4-byte unsigned path length, an 8-byte unsigned
object length, the UTF-8 path, and the object body. Integers are network byte
order. The body ends immediately after the declared final frame.

Version 1 bundles every immutable key because the service treats contents as
opaque. The client validates every frame in a temporary local remote before
resolving/checkout. The current service caps a bundle at 65,536 objects.
Clients fall back to raw/list on `404` or `405`.

### 8.4 Receive pack and receive bundle

`receive-pack` streams one repository object pack and index, creates both
immutably, then performs the ref CAS. A malformed/truncated body MUST NOT update
the ref. Existing equal immutable pack/index objects are idempotent.

Its request headers are:

```http
x-graft-pack-id: <64 lowercase hexadecimal characters>
x-graft-pack-bytes: <decimal byte length>
x-graft-index-bytes: <decimal byte length>
x-graft-ref-replacement-hex: <lowercase exact ref bytes>
x-graft-expected-present: true|false
x-graft-expected-hex: <lowercase exact expected bytes, or empty>
```

The body is exactly `pack || index`, and `Content-Length` equals the two
declared lengths.

`receive-bundle` first streams a manifest containing one through the current
limit of 256
immutable snapshot/payload objects, then pack, index, and finally ref CAS.
Manifest paths are unique immutable keys with exact lengths and an
`allow_existing` collision policy. Truncated or trailing bytes MUST prevent ref
publication. Clients fall back to individual puts plus receive-pack/CAS.

It adds `x-graft-bundle-manifest-bytes`; the UTF-8 JSON manifest is:

```json
{
  "version": 1,
  "objects": [
    { "path": "segments/example", "bytes": 4096, "allow_existing": true }
  ]
}
```

The body is exactly `manifest || each object in manifest order || pack ||
index`. `allow_existing: false` returns `412` on collision so the client can
read/verify and use the individual fallback.

### 8.5 Multipart immutable objects

Multipart transfer is optional and changes only transport, not object identity.
Start/resume binds an opaque upload ID, target key, total length, and part size.
Parts are numbered from one; replacing a part is retry-safe. Complete atomically
exposes the assembled immutable object only after every exact part is present.
Abort removes incomplete session state. Multipart completion never publishes a
ref.

The route headers are:

```http
POST   /multipart-start/<key>     x-graft-object-bytes: <total>
PUT    /multipart-part/<key>      x-graft-upload-id: <id>
                                  x-graft-part-number: <positive integer>
POST   /multipart-complete/<key>  x-graft-upload-id: <id>
DELETE /multipart-abort/<key>     x-graft-upload-id: <id>
```

Start returns `upload_id`, `total_bytes`, `part_bytes`, and uploaded part
metadata. All non-final parts have exactly the advertised part size; the final
part is the remainder. Repeating start for the same key and total size resumes
the durable session. The current native client supports at most 10,000 parts
and attempts an individual missing part up to three times.

## 9. Refspecs and remote-tracking refs

A refspec has optional leading `+`, a source, `:`, and a destination. It may be
exact or contain exactly one `*` in both corresponding sides. Fetch destinations
must remain under `refs/remotes/<remote>/...`; push destinations are branch refs
under `refs/heads/...`. Push deletion uses an empty source and an exact
destination. Ambiguous, escaping, or mismatched wildcard refspecs MUST fail.

Leading `+` permits non-fast-forward update for the explicit mapping. Without
it, push MUST reject non-fast-forward replacement. Refspec force is scoped to
that mapping and does not disable expected-value CAS.

## 10. Fetch, push, pull, and clone

### 10.1 Fetch

Fetch resolves selected remote branch refs, downloads the required repository
commit graph/object data, and updates only `refs/remotes/<remote>/...` through
expected-state publication. It MUST NOT move the local branch, change the
index, merge, or materialize the worktree.

Fetch need not hydrate every SQLite segment or external payload. Historical
metadata can therefore be available while a later content/row operation still
performs hydration or payload fetch.

### 10.2 Push

Push validates local source and observed remote destination. It rejects a
non-fast-forward update unless explicitly forced. If the remote already names
the same commit, it succeeds without unnecessary publication.

Publication order is:

1. prepare and publish SQLite commits/segments and external payload bytes;
2. publish repository objects or pack/index;
3. atomically CAS the destination ref from the exact observed value.

Thus a failed ref CAS may leave unreachable immutable objects but cannot expose
an incomplete commit. Branch deletion uses CAD against expected bytes. Safe
deletion MUST reject a mismatch with known remote-tracking state unless force
policy explicitly allows a freshly observed expected value.

### 10.3 Pull

Pull is fetch followed by merge planning/application into the current branch.
It inherits fetch's remote-tracking update and merge's up-to-date,
fast-forward, three-way, conflict, stale-token, and materialization behavior.
It MUST NOT implement an independent overwrite algorithm. The HTTP client MAY
reset transport pools between object fetch and snapshot hydration without
changing semantics.

### 10.4 Clone

Clone initializes a new repository, configures the named/default remote,
obtains a stable selected ref through upload-bundle or raw/list fallback,
validates imported immutable data, creates remote-tracking/upstream state, and
checks out the selected branch according to repository/materialization rules.

Failure before final repository publication MUST leave either no usable clone
or a discoverably incomplete temporary destination; it MUST NOT claim a valid
branch pointing to missing objects. Existing non-empty destinations are not
silently overwritten.

## 11. Status, retries, and publication uncertainty

Important HTTP status meanings are:

| Status | Meaning |
| --- | --- |
| `400` | malformed path/query/header/body |
| `401`/`403` | authentication/authorization |
| `404` | missing repository/object or unsupported optional route fallback |
| `405` | operation disallowed/optional operation unavailable |
| `409` | expected ref bytes did not match |
| `412` | create-only immutable collision |
| `413`/`414` | body/key limit |
| `416` | invalid range |
| `423` | optional transient lock contention |
| `426` | protocol mismatch |
| `429` | rate limit |
| `500`/`503` | service failure/unavailability |

Errors SHOULD be `application/problem+json`, but clients rely on status and
MUST tolerate unknown bodies.

Reads and idempotent immutable creates MAY retry with bounded backoff. A ref
CAS mismatch is a known rejection, not a transport retry. If a connection fails
after a ref publication request may have reached the service, the adapter MUST
distinguish known rejection, publication unconfirmed, and outcome unknown. It
must inspect/reconcile expected remote state before issuing a different update.

Cancellation has the same safe-boundary rule: it may prevent unsent work, but
cannot assert that an in-flight publication did not happen.

## 12. Service consistency and conformance

A conforming remote service provides durable read-after-write for successful
mutations, byte preservation, atomic create-only writes, linearizable per-key
CAS/CAD, repository isolation, and complete stable-prefix listing in the
absence of concurrent mutation.

A `GRAFT-Remote-1.0` implementation MUST test:

1. URI normalization and credential redaction/rejection;
2. protocol header negotiation and key validation;
3. full/range/head/list operations and cursor progress;
4. immutable collision and exact CAS/CAD semantics;
5. malformed/truncated bundle/pack/multipart rejection before ref update;
6. capability fallback paths;
7. exact/wildcard/force/delete refspec validation;
8. fetch isolation from local branch/index/worktree;
9. push ordering and concurrent-ref rejection;
10. pull equivalence to fetch plus merge;
11. clone validation and destination safety; and
12. timeout/cancellation publication uncertainty recovery.

Current evidence lives in `crates/graft/src/repo/sync.rs`, remote backends and
runtime actions, `packages/graft-remote`, its Hono/Cloudflare adapters and test
suites, plus CLI/SDK remote integration tests.

## 13. Compatibility notes

- Legacy `/api/graft/v1/repos/...` bases may remain aliases, but clients treat
  configured bases as opaque and never insert that prefix.
- Optional aggregate operations always retain raw/list/CAS fallbacks.
- Version 1 upload bundles enumerate all immutable keys; reachability
  negotiation is a future protocol concern.
- HTTP, filesystem, and S3 backends must share publication semantics even when
  their internal transaction mechanisms differ.
