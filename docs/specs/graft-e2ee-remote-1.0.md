# Graft E2EE Remote 1.0

Status: Final design specification; opt-in Unix native source implementation; production validation pending
Version: 1.0
Design date: 2026-10-12
Canonical language: English
Profile: `encrypted-remote-v1`

## Abstract

This specification defines client-side encryption of Graft HTTP remotes with
server-readable opaque synchronization hints. Contents, paths, branch names,
commit messages, and authoritative indexes are encrypted. The server can see
opaque commit ancestry, commit-to-pack membership, and ref routing targets so
that it can select encrypted packs and preserve aggregate synchronization.

A random repository root key is wrapped by a key derived from a separate sync
password. Trusted hosts may remember the root key in system-protected storage.
An authorized device can change the password without reencrypting history.

Requirements are the implementation target, not evidence of current support.
Production claims require Section 20 evidence and independent security review.
The key words MUST, MUST NOT, SHOULD, SHOULD NOT, and MAY follow BCP 14.

## 1. Scope and ownership

This document owns the encrypted profile, cryptographic encodings, disclosed
routing metadata, key lifecycle, and encrypted HTTP extensions. It specializes
[Remote Sync 1.0](./graft-remote-sync-1.0.md) without changing plain remotes.
[Repository](./graft-repository-1.0.md),
[Storage](./graft-storage-snapshots-1.0.md),
[Merge](./graft-merge-1.0.md), and
[Worktree Materialization](./graft-worktree-materialization-1.0.md) retain their
ownership of object identity, snapshots, integration, and physical files.

Version 1 targets personal multi-device Rust, CLI, Rust/Node SDK, and trusted
native hosts such as Lite using HTTP. Fs/S3, browser synchronization, team key
distribution, device pairing, recovery-code slots, signatures identifying
authors, and in-place root-key rotation are outside this version.

Encryption is selected when creating a remote. A remote MUST have one fixed
profile and UUID; it MUST NOT mix plain and encrypted namespaces. Local SQLite
and ordinary files remain plaintext. Locking sync MUST NOT be presented as
encrypting or locking local files. Plain-to-encrypted conversion requires a new
remote and Section 18 migration.

## 2. Threat model and disclosure

The server may read, change, substitute, replay, omit, fork, or delete stored
data. Clients, their cryptographic RNG, and unlocked devices are trusted.

| Information | Treatment |
| --- | --- |
| File/SQLite content, logical paths, branch names, commit messages, full pack indexes and offsets | Client-side encryption |
| Passwords, root/derived keys | Never sent to the server |
| Account, random UUID, profile/suite, object class, size/count, time and repeated access | Visible |
| Opaque commit parent edges and commit-to-pack membership | Intentionally visible |
| Opaque ref target, branch movement, symbolic HEAD target, annotated-tag ref kind and routed commit, merge topology | Intentionally visible |
| Equality within a repository, batch/request grouping, orphan objects | Observable |
| Plaintext content hashes and IDs | MUST NOT be exposed |

Opaque identifiers prevent direct computation without repository keys; they do
not hide topology or all activity. A chosen-upload oracle plus observation may
confirm content guesses. There is no cross-repository deduplication.

AEAD and authenticated hints detect modification and identity substitution.
Local configuration checkpoints detect rollback of already observed config
versions. This version does NOT provide complete ref rollback detection,
global freshness, fork detection, list completeness, availability, or secure
erasure of past copies. First enrollment trusts the successfully authenticated
configuration and cannot prove it is the latest.

All root-key holders share one cryptographic write authority. Symmetric MACs
do not identify authors. Local plaintext, compromised unlocked devices,
authorized plugins, malicious client updates, and memory dumps are outside the
confidentiality guarantee. Account access control is separate from key access.

## 3. Layering and mandatory coverage

```text
Repository / Storage / Sync / Merge
             ↓ logical keys, plaintext, observations
       PlainRemote | EncryptedRemote
             ↓ opaque keys, ciphertext, routing hints
             OpaqueTransport
             ↓
        HTTP server / storage

Trusted host KeyProvider → native SecretSession → EncryptedRemote
Account CredentialProvider → OpaqueTransport
```

EncryptedRemote MUST cover raw/commit/segment reads and writes, ranges,
hydration, discovery, clone, batch endpoints, and multipart. No optimization or
background storage action may bypass it. Missing keys, modules, configuration,
or support MUST fail closed, never fall back to plain transport data.

Graft owns the encryption and authenticated planning contract. Hosts own input,
system storage, fresh local authentication, UI, and migration coordination.
Servers own account authorization, quotas, byte transport and atomic CAS/CAD.
They MUST NOT require plaintext indexes or keys.

Transport, credential and trusted key-provider interfaces MAY be compiled-in
extensions. Arbitrary JavaScript encryption callbacks, dynamic algorithm
plugins, and ordinary-plugin access to KeyProvider are not permitted.

## 4. Primitive suite and canonical encoding

Suite 1 is XChaCha20-Poly1305 with 16-byte tags and 24-byte nonces,
HKDF-SHA-256, HMAC-SHA-256, SHA-256, and Argon2id v1.3. All randomness MUST use
the operating system CSPRNG. Unknown suite/format/KDF IDs MUST be rejected.

`U8`, `U16`, `U32`, and `U64` encode unsigned big-endian integers. All byte lengths
exclude their own prefix. `E(parts...)` is U16 field count followed, for each
field, by U32 byte length and raw bytes. An integer field contains its exact
fixed-width encoding. Nested lists use E over encoded entries; an empty list
is E with zero fields. `||` means byte concatenation. UUIDs use 16 raw bytes.
IDs use 32 raw bytes inside binary structures and 64 lowercase hex characters
in URLs/JSON. Text is valid UTF-8 without normalization or case folding.

Parsers MUST require exact field counts, order, widths, enums, and end of input;
reject duplicates where uniqueness is specified; bound lengths before
allocation; and use checked arithmetic. Logical keys MUST satisfy the existing
Graft rules and be at most 4096 UTF-8 bytes. Limits are Section 16 maxima.
Each cryptographic operation MUST use the domain label specified below.

## 5. Key hierarchy and passwords

Creation generates UUIDv4 and a random 32-byte `K_root`. For each purpose,
derive `HKDF-SHA-256(IKM=K_root, salt=UUID,
info=E("graft/e2ee/v1", profile, U16(1), purpose), L=32)`.
`profile` is the UTF-8 string `encrypted-remote-v1`.

| Purpose | Key use |
| --- | --- |
| name | Logical object name HMAC |
| header | Per-upload header key derivation |
| payload | Per-upload payload key derivation |
| ref | Mutable ref/HEAD AEAD |
| config | Configuration MAC |
| route | Opaque commit routing IDs |
| hint | Public hint MAC |

KDF profile 1 is Argon2id version 0x13, m=65536 KiB, t=3, p=4, 32-byte output,
32-byte random salt, no Argon2 secret or associated-data inputs. Implementations
MUST NOT silently lower these parameters. Fewer worker threads are allowed if
the lane count and result remain identical. KDF concurrency is bounded.

Passwords MUST be valid raw UTF-8 of 1–1024 bytes, without trim or Unicode
normalization. JavaScript inputs MUST reject unpaired surrogates. Hosts SHOULD
recommend password-manager-generated strong passwords, permit paste, and
preserve exact input. Wire acceptance of a short password is not a strength
recommendation. Password guessing is possible offline against the wrapped key.

`K_wrap = Argon2id(password, salt, profile_1)`. Passwords and K_wrap MUST NOT be
persisted by default and SHOULD be zeroized when no longer needed.

## 6. Configuration and enrollment

`e2ee/config` is a fixed 206-byte mutable object:

| Field in wire order | Width |
| --- | --- |
| ASCII magic `GRAFTEC1` | 8 |
| format=1, suite=1 | U16 each |
| UUID | 16 |
| generation, initially 1 | U64 |
| mutation_id | 32 random bytes |
| kdf_profile=1 | U16 |
| salt | 32 random bytes |
| wrap_nonce | 24 random bytes |
| wrapped_root including tag | 48 |
| config_mac | 32 |

Let C0 be the exact concatenated fields through salt, C1 the exact fields
through wrapped_root. Wrap K_root using K_wrap with AAD
`E("graft/config-wrap/v1", profile, C0)`.
`config_mac = HMAC-SHA-256(K_config,
E("graft/config-mac/v1", profile, C1))`.

Before KDF, clients MUST validate lengths, IDs and pinned identity. After
unwrap they MUST validate the complete MAC. Incorrect password or unwrap/tag
failure returns UnlockFailed without distinguishing password from damaged
wrapping. A root-key session verifies the config MAC without the old password.

After enrollment, clients MUST pin UUID/profile/suite and persist a private,
crash-safe checkpoint `(UUID, generation, SHA256(config_bytes))`. Lower
generation or equal generation with different digest MUST fail. A higher
authenticated generation may advance the checkpoint; it is not proof of a
unique successor or globally latest config. Generation overflow MUST fail.
Checkpoint loss requires explicit local trust reconstruction.

Checkpoint validation and durable advancement MUST succeed before admitting
network data operations under the observed configuration. Storage failures MUST
return CheckpointUnavailable, without updating in-memory trust or caching a
verified session epoch. Persistence tasks that own keys remain within the session
lock/drain boundary even if their asynchronous caller is abandoned.

First enrollment establishes trust by successful unwrap/MAC validation, not
by a server statement. An enrolled remote losing its config MUST return
RemoteConfigMissing, never be initialized again or downgraded.

## 7. Logical names and encrypted objects

For normalized logical key L and class C:

```text
T = HMAC-SHA-256(K_name, E("graft/name/v1", profile, C, L))
remote_key = "e2ee/" + C + "/" + lowercase_hex(T)
```

Classes are `objects`, `packs`, `indexes`, `files`, `commits`, `segments`,
`refs`, and `head`. The logical classifier MUST preserve existing Graft key
identity; it MUST NOT merge different keys with similar content. For legacy
logical keys, `objects/pack/*.pack` maps to packs, `objects/pack/*.idx` to
indexes, other `objects/**` to objects, `store/files/**` to files,
`logs/<log>/commits/<lsn>` to commits, `segments/**` to segments, `refs/**` to
refs, and `HEAD` to head. Unknown logical classes MUST be rejected.

An immutable ciphertext begins with a 72-byte public prefix: ASCII
`GRAFTEO1`, U16 format=1, U16 suite=1, 32-byte random upload_id, 24-byte random
header_nonce, U32 encrypted_header_length. The encrypted header, including its
tag, is at most 16 KiB.

Derive per-upload keys with HKDF-SHA-256: IKM K_header or K_payload, salt
upload_id, info respectively
`E("graft/upload-header/v1", profile, UUID, remote_key)` or
`E("graft/upload-payload/v1", profile, UUID, remote_key)`, L=32.

Header plaintext is E(logical_key, class, U64 plaintext_length,
32-byte plaintext_sha256, U32 chunk_size, U64 chunk_count, U32 flags=0).
Header AAD is `E("graft/object-header/v1", profile, UUID, class, remote_key,
public_prefix)`.

Payload uses chunks of 16/64/256 KiB; default is 64 KiB. The nonce for chunk i
is 16 zero bytes || U64(i). Its AAD is
`E("graft/object-chunk/v1", profile, UUID, remote_key, upload_id,
SHA256(encrypted_header), U64(i))`. A new encryption attempt MUST generate a
new upload_id and header nonce. Different plaintext MUST NOT reuse upload_id.

Chunk count is max(1, ceil(plaintext_length/chunk_size)); empty objects have
one authenticated empty chunk. Non-final chunks have exactly chunk_size
plaintext bytes. Clients MUST validate key mapping, class, flags, lengths,
count and complete ciphertext size. Full reads MUST check plaintext_sha256
and Graft's authoritative identity. Segment IDs MUST NOT be assumed to be
plaintext hashes. Immutable bytes MUST never be overwritten.

## 8. Range reads and local caching

For [a,b), authenticate the bounded header, fetch all covering ciphertext
chunks, authenticate each, then slice. Let B be chunk_size and H the offset
after the encrypted header. Chunk i starts at `H+i*(B+16)`; the final size
follows authenticated plaintext_length. Empty ranges do not fetch payload;
out-of-bounds or overflow MUST fail.

Partial reads authenticate the header and covered chunks, not the full-file
digest. Header caches bind UUID/key/upload_id; chunk caches additionally bind
chunk index and authenticated status. Whole-file compression MUST NOT be
introduced by this encryption layer. Existing upper-layer layouts remain.

Four-KiB cold reads in a 64-KiB chunk can amplify bytes by about 16 times before
header overhead; performance claims MUST measure this cost. Senders MAY choose
another defined chunk size without changing plaintext identity.

Local decrypted caches follow the local-plaintext model with private file
permissions. Sync lock does not erase already delivered plaintext or encrypt
temporary SQLite databases. Server and upload spools SHOULD retain ciphertext.

A session-local discovery cache MAY retain bounded encrypted headers to recover
immutable logical identities during listing. It MUST be scoped to the pinned
account/repository/profile/suite, endpoint and admitted secret-session generation,
and MUST authenticate cached bytes before interpreting their metadata. Lock,
disconnect or replacement of the secret session MUST invalidate this cache;
stale operations MUST NOT repopulate it. A cached discovery identity MUST NOT
substitute for a fresh remote existence check, ref observation, collision check,
or payload authentication. Listing identity is not proof of current availability.

## 9. Authenticated opaque routing hints

### 9.1 Commit IDs and hint identity

For authoritative raw 32-byte commit ID c:
`route(c) = HMAC-SHA-256(K_route,
E("graft/route-commit/v1", profile, c))`.
These tokens MUST NOT replace authoritative Graft IDs inside encrypted data.

For pack P, its hint key is `e2ee/hints/<pack-token>`, where pack-token is the
64-hex suffix of P's opaque `e2ee/packs/` key. Its plaintext hint is immutable,
client-authenticated, and deliberately server-readable.

Hint prefix is ASCII `GRAFTEH1` || U16 format=1 || U16 suite=1 || UUID.
Body is E(opaque_pack_key, opaque_index_key, E(commit_entries)). Each entry is
E(route_commit_id, E(parent_route_ids)). Entries MUST be sorted by raw commit
token and unique; parents retain authoritative order and MUST be unique.
Each parent is 32 bytes; each entry has at most 32 parents. A hint has at most
4096 entries and total bytes including MAC at most 1 MiB.

Hint bytes are prefix || body ||
`HMAC-SHA-256(K_hint, E("graft/routing-hint/v1", profile, UUID, hint_key,
prefix, body))`. Readers MUST verify MAC, identity, limits, sorting, key classes,
and key/pack-token association. Reuse of a hint key requires exact byte equality.

Publishers MUST emit a complete hint for each published pack containing commits
when representable within these bounds. They SHOULD split such packs to remain
representable. A pack containing no commits needs no hint. Unrepresentable
historical topology MAY omit hints and use verified fallback; it MUST NOT
truncate parent relationships or change repository semantics to fit a hint.

### 9.2 Authority and server indexing

Hints are prefetch/discovery aids, never repository truth. Clients MUST compare
used hints against decrypted pack/index contents, recompute route tokens from
real commit IDs/parents, and verify authoritative object hashes. Every hinted
commit MUST occur in the associated pack. A client MUST complete verified
graph discovery even if the server claims that all data was returned.

Servers cannot verify the MAC and MUST treat hint parsing as untrusted bounded
input. Their derived graph index MAY map commit token → ordered parents and
one or more pack/index/hint tuples. The same commit in several packs is legal;
conflicting parent hints MUST disable that planning path rather than silently
select a topology. Servers SHOULD index newly published hints incrementally
and rebuild from hint objects; they MUST NOT require decrypting indexes.

The index is disposable. Missing/stale/incomplete hints, absent graph nodes,
unsupported topology, or unavailable hinted data require hint-unavailable
fallback. Clients MUST NOT interpret missing hints as missing history.

## 10. Routed refs and exact observations

A ref/HEAD has public prefix ASCII `GRAFTER1` || U16 format=1 || U16 suite=1 ||
U8 route_kind || 32-byte route_target || 24-byte random nonce (69 bytes).
route_kind 1 means a direct commit; route_target is route(target_commit).
route_kind 2 means symbolic HEAD; route_target is the raw token suffix of the
opaque target ref key. Only HEAD may use kind 2. Absent refs have no envelope.
route_kind 3 means an annotated-tag ref; route_target is route(tag_commit).
Only logical `refs/tags/...` may use kind 3. The authoritative tag object ID is
encrypted and MUST NOT be replaced by its commit ID.

Encrypted body is E(logical_ref, authoritative_target, 32-byte random
mutation_id). Both text fields use the existing normalized ref/target rules;
kind-1 targets contain the canonical lowercase Graft commit ID text.
Symbolic HEAD targets use `ref: refs/heads/<normalized_branch>` without a trailing
newline; direct targets likewise exclude a trailing newline. AAD is
`E("graft/ref/v1", profile, UUID, remote_key, public_prefix)` with K_ref.
Total envelope bytes MUST NOT exceed 16 KiB.

Kind-3 authoritative_target is exactly `tag: <tag_object_id> commit: <commit_id>`,
with each ID encoded as 64 lowercase hex characters, one space between the
specified fields and no trailing newline. Clients MUST authenticate the envelope,
recompute its routed commit and verify the actual encoded tag object's identity,
type and declared commit against both encrypted IDs before trusting this edge.
Tag bytes and complete commit dependencies MUST be published before final ref CAS.
Lightweight tags use kind 1. Opaque services accept kind 3 only under `e2ee/refs/`,
never `e2ee/head/`; they cannot validate the encrypted logical tag name or content.
Unsupported tag transport MUST fail without peeling the tag or falling back to
another route kind. A generic ref writer receiving only an ID MUST NOT guess
whether it denotes a commit or annotated tag; it requires typed/verified metadata.

Clients MUST authenticate then recompute the public route_kind/target from the
decrypted value. This binds visible routing to the exact encrypted ref; there
is no independently mutable ref-routing sidecar. Branch names remain encrypted.

RefObservation binds UUID, key, session generation and actual observed envelope
bytes or absent. CAS/CAD MUST compare these exact bytes. Reencrypting a
plaintext expected target is not a valid comparison value. Password changes
do not invalidate observations when root/session identity is unchanged;
locking or reconnecting invalidates session handles.

Deletion uses exact CAD, recreation uses absent CAS. HEAD initialization uses
create-if-absent; later changes use exact CAS. Multi-ref push remains individually
atomic and MUST report partial success, without a repository-global ref lock.

## 11. HTTP profile and aggregate framing

Every request/response retains `Graft-Protocol: 1`. GET base MUST declare
profile `encrypted-remote-v1`, encryption version=1, suite=1, UUID, supported
capabilities and limits. The authenticated config is the cryptographic identity
authority; descriptor disagreement MUST fail. Production MUST use TLS; only
isolated loopback testing may use cleartext HTTP. Bearer credentials MUST NOT
follow cross-origin redirects.

The descriptor uses the existing top-level `protocol`, `version`,
`capabilities`, and `limits`, plus the exact `encryption` object below. Its UUID
is the canonical lowercase hyphenated UUIDv4 text corresponding to config's
16 bytes; `encryption.version` is separate from the HTTP protocol version.

```json
{
  "protocol": "graft-remote",
  "version": 1,
  "encryption": {
    "profile": "encrypted-remote-v1",
    "version": 1,
    "suite": 1,
    "uuid": "123e4567-e89b-42d3-a456-426614174000"
  },
  "capabilities": [
    "range", "list", "list-cursor", "put-if-absent", "cas", "cad",
    "e2ee-read-bundle", "e2ee-fetch-bundle", "e2ee-receive-bundle",
    "e2ee-multipart-object"
  ],
  "limits": { "max_request_bytes": 67108864, "multipart_part_bytes": 8388608 }
}
```

The two byte limits are an example, not mandatory transport defaults. Existing
descriptor unknown-field handling applies; clients MUST validate all required
encryption fields and reject unknown encryption version/suite/profile.

Encrypted servers MUST reject plain keys and old semantic bundle routes for
encrypted repositories. Valid keys are config, the Section 7 opaque keys, and
Section 9 hint keys. Clients MUST explicitly select/pin this profile; an older
plain client MUST NOT silently open it as Graft Remote 1.0.

Unconditional raw PUT/DELETE MUST be disabled for this profile. Only config,
refs and head accept CAS/CAD; immutable object and hint creation uses
create-if-absent or receive/multipart completion. Mutable entries MUST NOT be
accepted as immutable batch frames. Servers check envelope structure, declared
profile identity and key class before mutation, without claiming MAC validation.
JSON control bodies MUST reject duplicate or unknown fields and invalid types.

Required base capabilities are range, paginated list, immutable create-if-absent,
exact CAS/CAD, and `e2ee-read-bundle`. Optimized synchronization adds
`e2ee-fetch-bundle`, `e2ee-receive-bundle`, and `e2ee-multipart-object`.

| Endpoint relative to base | Contract |
| --- | --- |
| Existing head/raw/range/list/put-if-absent/cas/cad routes | Opaque keys, exact byte operations; config/ref mutation as allowed |
| POST /e2ee/read-bundle | Explicit opaque immutable keys; ciphertext and/or public authenticated hints |
| POST /e2ee/fetch-bundle/<opaque-ref-key> | Routed selected-ref discovery and bounded encrypted pack/index/hint batch |
| POST /e2ee/receive-bundle/<opaque-ref-key> | Immutable encrypted dependency/pack/index/hint batch, then exact routed-ref CAS |
| Existing multipart routes with e2ee-multipart-object capability | Section 14 upload identity and ciphertext semantics |

Opaque-ref-key is the existing per-segment URL encoding of its full opaque key.
Only refs keys are valid fetch/receive targets; symbolic HEAD is resolved to an
authenticated ref by the client. Unknown optional capability absence permits
only encrypted primitive fallback, never plain semantic routes.

Aggregate binary framing is ASCII `GRAFTEB1` || U32 manifest_length || manifest
|| frames. Manifest is E(U16 version=1, reference_key_or_empty,
reference_bytes_or_empty, E(frame_descriptors)). Each descriptor is
E(opaque_key, U64 length). Each frame is U32 key_length || U64 object_length ||
key UTF-8 bytes || exact object bytes. Descriptor/frame keys MUST be unique,
bytewise sorted and equal in order/length; no trailing bytes are allowed.
The frame count is the nested list's exact field count. Manifest is at most
256 KiB, reference values at most 16 KiB, and all frames follow Section 16.

Responses declare exact object count and framed bytes using
`x-graft-bundle-objects` and `x-graft-bundle-total-bytes`. Content-Length SHOULD
match when available; actual received bytes MUST always be checked. Requests
with bodies MUST declare exact Content-Length. Read/fetch response Content-Type
is `application/vnd.graft.e2ee-bundle`; receive request uses the same type.

## 12. Fetch, pull, clone and fallback

Read-bundle JSON is exactly `{ "version": 1, "keys": [opaque_key, ...] }` with
1–256 unique immutable keys, including hints. Its response has empty reference
fields. Missing objects fail the whole batch; clients MAY retry bounded
individual reads. A read-bundle cannot read mutable config/refs.

Fetch-bundle JSON is exactly `{ "version": 1, "have": token_or_null }`.
have is the 64-hex route token of the last locally verified fetched commit.
Clients MUST NOT advertise a token merely observed in an unverified hint.

The server reads exact routed ref bytes, traverses its route_target graph until
have or known roots, selects covering pack/index/hint tuples, then rereads the
ref. It MUST return 409 if the bytes changed, 422 if hint coverage/planning is
unavailable or limits are exceeded, and never an alleged empty-history result
for missing graph data. Cycle detection and bounded traversal are mandatory.
If have equals route_target, it may return zero frames with the exact ref.
This is a hint only: the client still checks the actual decrypted ref target.

The response carries selected encrypted packs, encrypted indexes, complete
public hints, and the observed routed ref in the manifest. Clients MUST verify
framing, ref AEAD/routing, hint MACs, object envelopes, plaintext identities
and graph reachability. They MUST discover any omitted authoritative
dependencies using encrypted operations before confirming fetch completion.
The manifest reference_key MUST equal the requested opaque ref key; an absent,
different, or unauthenticated returned reference MUST fail.

Missing capabilities, 404/405/413/422 and explicit 409 races permit bounded
encrypted discovery/retry. Auth failures, malformed framing, hint MAC failures,
or object-integrity failures MUST NOT be silently downgraded to successful
fallback. Authenticated hints inconsistent with verified plaintext MUST be
reported as HintMismatch; explicit verified retry may ignore those hints, but
no success result may conceal the discrepancy.

Primitive discovery reads and authenticates the selected ref, lists opaque
indexes in bounded pages, decrypts required index/object metadata, and plans
pack reads locally using cached verified data. It MUST NOT trust list completeness
or fetch unrelated plaintext worktree state. Cold discovery can be O(N);
ordinary cached/optimized fetch SHOULD avoid rescanning all indexes.

Fetch MUST NOT move a local branch, index or worktree. Pull remains fetch plus
existing local merge. Clone fixes a verified selected ref target and obtains
its complete required closure; a changing branch may make aggregate fetch
unavailable, in which case primitive reads may continue from the fixed immutable
target. Clone destinations MUST be absent or empty directories; an existing
nonempty destination MUST be rejected before replacing files. A native clone
using an explicitly registered remote name MUST retain that name's live
credentials, pinned encrypted identity and session lock/disconnect lifecycle.

## 13. Push and encrypted receive

Clients MUST construct and locally validate ciphertext and hints before
publication. Publish immutable snapshot/payload dependencies, pack and full
encrypted index, then authenticated hints, then the exact routed-ref CAS.
No ref may reference an incompletely published dependency set.

Receive-bundle request manifest carries the opaque ref key and candidate ref
bytes; its sorted frames contain at most 256 immutable objects/hints. Headers
`x-graft-expected-present` and `x-graft-expected-hex` encode observed ref bytes
or absent exactly as in Remote Sync; replacement resides only in the manifest.
Servers MUST parse bounded identities/lengths, receive complete immutable frames,
reject truncation/trailing bytes, and perform the final ref CAS only after all
entries are available. They cannot verify client MACs or plaintext correctness.
Clients MUST perform those checks before sending.

Equal existing ciphertext/hint bytes may be reused. Different ciphertext for
the same immutable key MUST return 412 and prevent final CAS, never overwrite.
Random encryption can produce a legitimate byte mismatch for equal plaintext.
The client MUST authenticate the existing object's header, logical key, length,
plaintext digest and total ciphertext size before deciding it is reusable.
Header proof does not establish full remote availability. Hints require exact
byte equality and valid MAC; conflicting authenticated hints are an error.

After a benign collision the client MAY publish remaining objects with encrypted
primitives and then perform exact ref CAS. A server MUST NOT decide plaintext
equivalence by comparing random ciphertext or expose a plaintext digest API.
Already uploaded immutable objects may remain after any failure; the ref MUST
not advance on framing, collision, or incomplete-upload errors.

Unchanged verified head SHOULD avoid redundant publication. Existing verified
object caches SHOULD reduce upload probes. Different refs proceed independently;
same-ref races use existing fast-forward/merge rules, not unconditional overwrite.

## 14. Multipart, retries and uncertain outcomes

CAS/CAD expected-byte mismatches use HTTP 409 under Remote Sync 1.0; create-only
collisions use 412. Clients MUST recognize the appropriate mismatch response
when reconciling ref or config mutations and MUST authenticate readback before
classifying the current value. A response status alone does not confirm the
candidate or authenticate a competing value.

Multipart start additionally binds UUID, opaque key, object encryption upload_id,
total ciphertext bytes and SHA256 of complete ciphertext. Hints are small and
do not use multipart. Server session_id and encryption upload_id are distinct.
Resume MUST match every identity field and retransmit identical ciphertext.
The old key/length-only resume policy MUST NOT be used for encrypted objects.
Part boundaries follow server transport limits, not plaintext chunk boundaries.

Start requests carry `x-graft-e2ee-uuid` (canonical UUID text),
`x-graft-e2ee-upload-id` (64-hex encryption upload_id), and
`x-graft-e2ee-ciphertext-sha256` (64-hex digest), in addition to existing key and
total-byte fields. These identity fields MUST be recorded and bound to the
returned session_id, and echoed in start/resume responses for client validation.
Complete MUST check the actual assembled ciphertext length and SHA256 before
making the immutable object visible. This hash is not a plaintext identity
proof. Servers MUST NOT merge sessions with different encryption upload IDs.

Clients retain ciphertext spool or a protected record sufficient to regenerate
the identical immutable plaintext snapshot, upload_id and nonce. Complete uses
atomic create-if-absent, never makes partial objects visible; competitors verify
the winning existing object. Abort/TTL may clear incomplete sessions only.

Servers MUST serialize part-write admission with completion: an acknowledged
part cannot be frozen while a replacement write is in flight. A write whose
outcome is unknown MUST remain pending across restart until reconciled or
explicitly aborted. Immutable publication MUST atomically check that the exact
upload session remains active and valid; a verifier that started before an abort
or rejection cannot publish afterward. Abort MUST NOT delete a published winner.
An explicit abort may retire a frozen but unpublished session, including one
whose underlying storage upload expired, allowing a new server session with the
same retained ciphertext identity. Clients MUST confirm that abort before
restarting and resend every part to the new session.

Each ref/config update freezes observed old bytes, candidate new bytes and
mutation_id. If a response is lost, reread: candidate bytes confirm success;
old bytes permit retry of the same request; another value is confirmed conflict
only with evidence of mismatch, otherwise OutcomeUnknown. Read failure is
OutcomeUnknown. A successor commit does not prove this request succeeded.
Remote-tracking refs update only after confirmed results.

Cancellation or lock cannot prove an already transmitted mutation had no effect.
Persistent recovery journals MUST contain no plaintext passwords/keys; secret
material resides only in approved providers. Do not reencrypt a retry candidate.

Restoring a retained ref candidate is an explicit trusted-host action, not
revival of an invalidated session handle. Journals MUST bind records to the
application/account/repository identity. The current admitted session MUST
authenticate stored old and replacement ref envelopes and verify their opaque
key/logical-ref association before binding a new observation handle. Missing or
corrupt records MUST fail, never silently create a replacement candidate. A
durable journal bind MUST precede mutation dispatch. Key-owning persistence work
MUST remain inside the session lock/drain boundary if its caller exits.

An active per-ref recovery slot MUST NOT be replaced by a different operation
while its outcome remains unknown or unconfirmed. Retirement requires confirmed
publication or conclusive conflict, exact comparison with the retained candidate
and durable acknowledgement. A delayed retirement MUST NOT clear a newer
candidate. Independent refs MUST retain independent recovery slots.

Recovery namespaces MUST distinguish explicit first enrollment from reopening.
Durable identity-bound allocation evidence MUST precede admission of a new slot.
An admitted slot's missing directory or record MUST fail closed, including after
process restart; reopening a missing or recreated namespace MUST NOT silently
enroll it. Missing or corrupt allocation evidence MUST stop mutation admission.
Completion of an interrupted empty first allocation requires durable evidence
that the slot was never admitted. Admission MUST become durable before returning
the slot to any caller. Existing active or corrupt records MUST NOT be replaced
by empty initialization. Legacy allocation evidence without an admission state
MUST be treated as potentially admitted. Absence alone cannot prove that a
previously admitted candidate never existed.
Namespace allocation locks MUST NOT be held during remote publication.

Before binding encrypted credentials to a repository, trusted hosts MUST validate
that checkpoint, recovery and disposable staging directories are separate from
the actual worktree and repository metadata. Validation MUST resolve path aliases
and parent repository discovery; containment in either direction is overlap.
It MUST precede runtime mutation or remote requests. Creating a new destination
MUST NOT require creating filesystem state merely to validate its placement.
Hosts MUST maintain this separation when changing targets or filesystem layout.

## 15. Password changes, storage and session lifecycle

Forgotten-password reset authorization MUST be supplied by a trusted native host
for the exact application, authentication domain, account and repository binding.
It MUST require a fresh OS authentication attempt; account login, remembered-key
retrieval, cached consent or an ordinary plugin/JS assertion MUST NOT substitute
for it. The authorizer MUST NOT receive the root key. Authorization and subsequent
rewrapping MUST retain admitted session ownership through lock/drain, including
caller cancellation, and MUST reject a session invalidated during authorization.
Preparing a reset candidate does not publish it or confirm password recovery.

Password change requires an unlocked root-key session and fresh trusted host
authorization: local system authentication or reentry of the current password.
Account login alone is insufficient. Recovery without the old password requires
both usable root key and credible local authentication.

Read and authenticate config/checkpoint; generate new salt, wrapping nonce and
mutation_id; increment generation; wrap the same K_root; verify locally; exact
CAS old config bytes. Update the checkpoint and report completion only on
confirmed success. Concurrent change losers MUST NOT automatically overwrite.
Other unlocked devices continue; newly enrolled devices use the new password.

Preparing a password-change candidate MUST reauthenticate against the freshly
observed current config, not a cached older password wrapper. Preparation alone
MUST NOT publish the candidate or advance its checkpoint. Retained transactions
MUST preserve exact expected and replacement config bytes and bind them to the
application/account/repository identity before mutation dispatch. Restoration
MUST authenticate both configs under the admitted root, require the same UUID and
exactly one generation increment, and MUST NOT generate new wrapping bytes.
Password KDF and local verification work MUST remain owned by the admitted
operation through cancellation and lock/drain, including caller abandonment.

Config mutation results MUST be reconciled against an authenticated read even
after a success response. Only the exact replacement confirms this candidate;
the exact expected config permits retry. A different value confirms conflict
only with CAS mismatch evidence; otherwise the result remains unknown. Missing,
corrupt, foreign or rollback/forked reads MUST NOT confirm success or retire the
transaction. Confirmation requires durable checkpoint advancement and exact
journal retirement; acknowledgement failure remains OutcomeUnknown. Different
pending candidates MUST NOT replace one another. Inspection without CAS may
advance local recovery state after confirmation, but MUST NOT mutate the remote.

Before retiring a config transaction, implementations MUST validate the observed
config against the latest shared durable checkpoint and the session checkpoint,
excluding concurrent checkpoint advancement until retirement finishes. A valid
losing conflict MUST NOT advance either checkpoint. A stale session's cached
checkpoint alone is insufficient; inability to perform this atomic validation
leaves the outcome unknown and retains the active transaction.

Retirement MUST retain the exact last candidate in identity-bound durable local
state until a newer transaction supersedes it, so lost retirement acknowledgement
can be recovered after reopening. A retired record is not an active transaction
and MUST NOT be treated as proof of success: recovery still authenticates the
current remote config and applies the confirmation rules above. Loading active
or retired recovery state MUST be atomic. Delayed retirement MUST NOT erase a
newer record. Binding a different candidate after retirement MUST reject an
expected generation lower than the retired replacement generation. Missing
records MUST NOT be interpreted as confirmed outcomes. Legacy empty records
provide no retirement evidence.

Old password plus old wrapping records can still recover K_root. Password
change is not device revocation. Service token revocation does not erase keys.

Remembered K_root MUST be protected by system Keychain/Keystore or a
system-protected wrapping key, bound to application/authentication domain,
account subject, UUID/profile/suite. Private credential files are outside the
repository, use owner-only permissions and crash-safe replacement, and MUST NOT
enter sync, plugin storage, logs or default diagnostic bundles. Weak fallback
such as Electron basic_text MUST be rejected. At-rest protection does not
imply hardware isolation or user-presence authentication.

Unavailable safe storage disables remembering; a host MAY offer explicit
session-only unlock. Native secret handles MUST be nonserializable and Debug
redacted. Node/FFI imports use short-lived controlled byte buffers, never
password/key JSON, argv or environment variables. Implementations SHOULD
zeroize buffers but MUST NOT claim all GC/library/dump copies can be erased.

Session states are Disconnected, Locked, Unlocking, Unlocked and Locking;
account authorization is separate. Lock MUST close admission, invalidate handles,
cancel and await key-owning tasks, then release/zeroize keys. It MUST NOT claim
Locked while background tasks still hold usable keys. Explicit lock cannot be
bypassed by automatic provider reload. Pause preserves keys. Disconnect/logout
clear session keys and tasks; remembered keys MAY remain, bound to the original
account, with automatic loading disabled until explicit reconnect/login.
Forget-device deletes the remembered entry. These actions retain local files.

Provider load/save/delete work MUST remain within the session task boundary even
if its caller cancels or exits. Loaded roots MUST authenticate the bound config
and durably advance its checkpoint before admission. Missing entries MUST be
distinguished from unavailable or unauthorized secure storage. Forget-device
MUST drain all sessions sharing the entry before deletion and prevent reconnect
until deletion exits. A failed deletion MUST NOT be reported as forgotten;
cancelling a save/delete caller does not prove that the OS mutation failed.

Offline startup may use validated local bindings/checkpoints, not claim current
remote validation. The first online operation MUST verify current config.
System lock/suspend SHOULD lock sync; resume respects saved-device policy.
Hosts MUST distinguish local-file access from remote hydration requiring unlock.

## 16. Resource limits, errors and quotas

| Bound | Maximum/default |
| --- | --- |
| Config | Exact 206 bytes |
| Ref envelope / encrypted object header | 16 KiB each |
| Hint | 1 MiB, 4096 commits, 32 parents/commit |
| Logical key | 4096 UTF-8 bytes |
| Read/receive frame count | 256 |
| Read-bundle framed response | 64 MiB |
| Fetch-bundle | 128 pack/index/hint tuples, 48 MiB total |
| Fetch-bundle frame count | 384 |
| Aggregate manifest | 256 KiB |
| Server graph planning | 100000 distinct commit tokens/request |
| Data concurrency / KDF concurrency | Default 4 / maximum 1 per process |

Receive bodies use advertised byte bounds; clients MUST stream large uploads or
use multipart, not buffer entire files. Smaller advertised bounds take priority;
larger ones cannot remove local maxima. Lists MUST be paginated and checked for
key validity, repeats and progress. Caches and loops MUST have bounded budgets
and cancellation checks. Servers MUST authenticate before expensive planning.

Quotas count actual stored ciphertext and public hints. Progress distinguishes
logical bytes and wire bytes. Version 1 MUST NOT automatically delete completed
immutable objects based on server-inferred reachability. Failed pushes may leave
orphans. Only incomplete multipart sessions may expire automatically; long-term
GC requires a separately specified client reachability/fencing protocol.

Required structured errors include Locked, UnlockFailed,
SecureStorageUnavailable, RememberedKeyMissing, UnsupportedEncryptedProfile, RemoteIdentityChanged,
RemoteConfigMissing, ConfigRollbackDetected, CheckpointUnavailable, AuthenticationFailed, HintMismatch,
HintUnavailable, ImmutableConflict, StaleObservation, Conflict, OutcomeUnknown,
MissingDependency, QuotaExceeded and Cancelled. HTTP fallback statuses do not
replace these client distinctions. Text/errors MUST NOT contain secrets or
automatically send decrypted paths/content to the server.

## 17. Initialization and recovery

The service must atomically register an empty remote identity/profile under
its account-management mechanism; the account API itself is outside this spec.
Clients MUST publish config create-if-absent before initial dependencies,
ref and HEAD. No initial ref/HEAD may overwrite a concurrent publication.
Only a verified readable initial branch permits a connected success state.
Initial publication MUST reject an observed branch pointing to a different
commit, including an ancestor of the selected source. It MUST NOT apply ordinary
fast-forward semantics to that branch. An absent branch permits create-if-absent;
the exact retained commit permits idempotent recovery without replacing the ref.

A private crash-safe initialization journal records UUID, candidate config
digest/bytes and prepared/config-published/objects-published/ref-published/
connected stages. Keys remain in session/safe storage. A racing loser discards
its unpublished root and explicitly enrolls in the winning remote; it MUST NOT
reuse its own root under the winner's identity. After a crash, unwrap the same
config with saved key or password, authenticate state and resume. Missing key
does not permit replacing a published config with a new root.

Native password-based preparation MUST distinguish explicit creation from
reopening. Creation MUST reject existing local config trust and MUST authenticate
the generated wrapper before durable journal enrollment. Reopening MUST unwrap
the retained generation-1 config, never generate replacement root/config bytes,
and reject missing or inconsistent existing state. KDF, root generation,
verification and persistence MUST remain inside unlock admission and lock/drain
ownership through caller cancellation. Local preparation does not publish config
or establish a connected repository.

Source-bound preparation MAY enroll the selected intent with the initial
candidate. In that case the prepared main record MUST bind the private
intent's digest from its first readable version; the intent MUST NOT be added
by reconstructing a later HEAD during recovery. Reopening at any recorded stage
MUST preserve the exact candidate and require the retained intent to match the
selected source. Later-stage reopening does not itself prove remote effects.

Before initial data publication, the trusted host MUST durably bind the journal
to the selected normalized branch and authoritative source commit. This private
intent MUST remain fixed across retries and reopen; it MUST NOT be reconstructed
from a subsequently changed local HEAD. Missing or inconsistent retained intent
MUST stop source publication and stage advancement. A changed source MUST fail
before remote discovery and be rechecked before publishing dependencies. Intent
metadata stays outside the repository, plugin storage and default diagnostics.

The source repository's persisted remote config MUST exactly match the trusted
authority's intended config before binding initialization intent, and MUST be
rechecked at publication. Independent verification using another local remote
name MUST retain the same pinned encrypted identity, endpoint, account and
session lifetime; it MUST NOT mutate source registrations or permit plain fallback.

The verification destination MUST be absent or empty and separate from the
source and host-private state directories. Hosts MUST validate it before binding
initialization intent or publishing remote data, and recheck before cloning.

Config retries MUST reuse the journal's exact bytes. A confirmed config alone
MUST NOT be reported as connected. After response loss, read back and authenticate
the exact candidate before marking config-published. An absent config permits
retry of that candidate only while still prepared; a config missing after
config-published MUST fail without reinitialization. A different config without
mismatch evidence leaves the publication outcome unknown. A create-if-absent
mismatch with another config indicates competition, but does not authenticate
the winner or authorize using the losing root. Retained checkpoints MUST be
validated before publication, including against a stale prepared journal.

Stage advancement MUST follow confirmation of the corresponding remote effect
and become durable before reporting success. Journal persistence that retains a
key-owning operation MUST remain inside the session's lock/drain boundary even
if its asynchronous caller exits. Failure to confirm durable advancement leaves
the initialization pending; it does not authorize replacement config generation.

If password is lost but an authorized device can retrieve root and perform
local authentication, reset wrapping in place. If root and password are both
lost, verified available local history may be uploaded to a new remote; current
files alone recover only current state. Ciphertext alone is unrecoverable.
Customer support/account-password reset MUST NOT provide a decrypting backdoor.

## 18. Migration and compromised-key replacement

Plain→encrypted or encrypted→new-root migration MUST create a new UUID/root
remote. Select all history or explicit refs' reachable closure, fix source refs,
obtain and verify every repository/storage/file dependency, reencrypt and publish
target refs. Pack repacking MAY change pack layout, never authoritative object
identity or selected history. Missing historical dependencies MUST stop complete
migration; no silent reduction to current files is permitted.

Source selection MUST retain exact annotated-tag object IDs, not only their peeled
commit targets. An explicit-ref selection MUST NOT silently add unrelated HEAD
history. HEAD may be pinned separately to detect source changes. A source inventory
and repository-object validation alone MUST NOT be reported as complete migration:
SQLite storage and external file payload dependencies still require verification,
followed by target publication and independent recovery. Ref revalidation detects
changes; it does not replace the final write fence.

A resumable migration MUST durably retain its immutable source selection, exact
refs/HEAD, source identity and dependency-closure fingerprint, bound to the chosen
target identity in host-private state. Recovery MUST revalidate the retained source
and MUST NOT substitute current HEAD or a newly selected closure. Missing, corrupt
or identity-mismatched allocated records MUST fail closed, never be recreated as
fresh plans. The source journal MUST NOT contain passwords or root keys, and is
not proof of target enrollment, dependency availability or publication completion.

Target-ref publication MUST use an explicit mapping covering every selected source
ref. It MUST preserve authoritative annotated-tag IDs and distinguish them from
commit dependencies. Remote-tracking names MUST NOT be implicitly mapped, and
duplicate targets, file/directory namespace collisions or branch/tag conversion MUST
fail before uploading. Target HEAD MUST name a mapped branch or a commit already
in the selected closure; it MUST NOT expand the selected history. A preflight plan
alone MUST NOT claim an implemented tag wire contract or successful publication.
An all-source inventory MUST retain an unborn symbolic HEAD branch as an absent
target even before its ref file exists. Mapping it MUST NOT manufacture a commit
or upload an envelope for an absent ref.

Before target publication, the exact mapping and target HEAD MUST be durably bound
to the frozen source record and chosen target identity in host-private state.
Recovery MUST restore that retained intent, reject noncanonical/inconsistent state
and revalidate the source; it MUST NOT regenerate mappings or HEAD from current
defaults. Source and target records MUST be distinguishable. This intent MUST NOT
contain passwords or root keys and MUST NOT itself count as target enrollment or
publication confirmation.

Before publishing a migration target configuration, the host MUST durably bind
the exact generation-1 initialization candidate bytes to the retained target
intent, including its frozen source. Recovery MUST authenticate that retained
config under an admitted root and require the exact same initialization candidate
and target intent. Missing, corrupt or inconsistent binding MUST stop publication;
it MUST NOT authorize generating a replacement root/config. This private binding
MUST remain distinguishable from source/target plans and MUST NOT advance config
trust or count as a remote enrollment receipt. The coordinator MUST serialize
this preparation before remote initialization; merely reading a local prepared
stage does not fence a concurrent initializer.

Migration-aware initialization MUST validate the retained config/target/source
binding before initial HTTP discovery and again before recording a confirmed
publication stage. Verification tasks that hold keys MUST remain within session
lock/drain ownership, and bounded source traversal MUST observe cancellation.
A failed validation after a remote effect MUST NOT be treated as undoing that
effect or authorize rebinding; the retained operation remains unconfirmed.

Dependency verification MUST check actual payload length/content and storage page
content against the selected identities. Cached storage hashes or hydration markers
alone MUST NOT prove availability or integrity. Unverifiable local representations
MUST fail rather than silently replacing source identities.

An independent new session MUST clone/hydrate the target and compare refs,
object identities, SQLite snapshots and ordinary files without relying solely
on source-side caches. Capacity estimation includes ciphertext and hints.
Final cutover MUST fence/pause old writes, catch up final changes and reverify
the fixed refs before switching devices. Without service fencing, hosts must
coordinate an explicit pause and disclose that offline devices cannot be forced
to stop local editing. Journals permit resume and source remains by default.

Old remote deletion requires separate authorization. New encryption does not
erase previous plaintext or old-key-readable backups. Compromise recovery also
revokes actual available service credentials and enrolls only trusted devices
into the new root. Device-specific revocation MUST NOT be claimed when the
service supports only account-wide tokens. Offline edits may later be imported
and merged by trusted clients without distributing the new key to revoked ones.

## 19. Host, plugin and publication contracts

Trusted hosts SHOULD present create-password, first-device unlock, remember,
password reset, pause, lock, forget-device and migration using the same names.
Normal remembered-device use SHOULD NOT require repeated sync-password input.
UI success states require the confirmed protocol result; uncertain updates stay
pending/unknown, never reported as completed.

KeyProvider and password changes MUST NOT be exposed through ordinary plugin
RPC. Plugins receive only host-authorized Runtime plaintext, never root/wrapping
keys or sync passwords. Data and external-network permissions remain separate.
Synced plugin packages/settings do not automatically grant device execution or
network access. Publishing decrypted data requires independent user authority
and is outside the remote confidentiality promise.

CLI secrets use no-echo interactive input, protected input handles or trusted
providers. Remembering across processes requires an approved system provider;
otherwise each session unlocks explicitly. CLI/SDK JSON returns status and
structured results only. Native SDK/host adapters MUST preserve this spec rather
than implementing encryption only around JS fetch/push calls.

## 20. Conformance and production gates

`GRAFT-E2EE-Remote-1.0` covers encrypted base operations, key/config/object/ref/
hint codecs, lifecycle and verified fallback. `GRAFT-E2EE-Sync-1.0` additionally
requires fetch/receive aggregates, incremental hint indexing and safe multipart.
Host remembering/local recovery claims require platform-specific storage and
authentication evidence. No existing implementation currently claims these.

Required evidence:

1. Independent crypto review; primitive vectors and reproducible cross-language
   codec vectors for config, objects, refs, hints, routing IDs and framing.
2. Unicode/password/KDF bounds, empty objects, all block sizes, range boundaries,
   cross-key/repository swaps, prefix/header/chunk/ref/hint tampering and truncation.
3. Forged hints, conflicting parents, duplicate commit placements, wrong pack
   membership, cycles, incomplete/omitted graph, stale caches and graph rebuild.
   Hints must never alter verified fetch results or skip dependency validation.
4. Same/different-ref concurrency, deletion/recreation, password-change races,
   equal-plaintext/different-ciphertext collisions, partial receive, multipart
   identity mismatches, response loss, unknown outcomes and cancellation.
5. Kill/reopen at every initialization/change/migration stage, explicit lock and
   stale handles, unsupported profiles and denied plaintext downgrade.
6. Real CLI/Node and claimed host tests for SQLite/WAL, ordinary files, history,
   clone/hydration, merge and active-handle worktree safety; mocks are not evidence.
7. Password loss/local reset, unsafe storage rejection, account switching,
   logout/forget, OS lock/resume and genuinely independent target restoration.
8. Request/storage/log/diagnostic scans with unique synthetic plaintext markers:
   no secrets, raw hashes, names or content may appear. Expected opaque topology
   must be separately documented and verified, not mislabeled a leak-free graph.
9. Comparable plain versus encrypted benchmarks: first clone, warm/cold small
   updates, long-offline pull, 4-KiB hydration, large attachments, 100000-object
   discovery and branch competition. Report P50/P95, requests, wire bytes, read
   amplification, CPU, peak memory, KDF delay and orphan storage. Measure 16/64/
   256-KiB blocks. No unmeasured slowdown percentage may be advertised.

Production gates are bounded streaming memory, no ordinary full-repository
reencryption/rescan on small cached updates, no global ref publication lock,
authenticated data only, recoverable confirmed publication, and budgets set
against measured plain baselines. Publish independent offline verification
tools and vectors. Single packet inspection is not proof of all-client safety.

The local paired regression policy is versioned separately from this wire
protocol. `graft-e2ee-performance-policy-v2`, recorded in
`scripts/e2ee-performance-policy.json`, applies these core bulk wall-time gates
to `initial_push`, `cold_clone_sync`, `long_offline_push` and
`long_offline_fetch`:

- Encrypted P50 MUST be at most `max(3 × plain P50, plain P50 + 500 ms)`.
- Encrypted P95 MUST be at most `max(3 × plain P95, plain P95 + 750 ms)`.

Warm-small core operations retain their 500 ms P95 ceiling. SDK and hydration
wall-time budgets, request/wire/RSS/orphan gates, and sampling remain unchanged:
three core rounds and five SDK/hydration rounds in each of the plain-first and
encrypted-first orders. The full matrix has 498 checks, including 18 added
core bulk P50 checks. Percentiles use nearest rank; these finite samples are
regression evidence, not a production tail-latency SLA. Source, summary and
acceptance records MUST identify the policy. Historical failures retain their
original budgets and outcome; changing policy requires a new complete run and
MUST NOT relabel earlier evidence or omit failed samples.

The design is fixed at version 1.0; interoperable implementations require exact
vectors and the review above. Changes to encoding, leakage, algorithms or atomic
semantics require an explicit version/profile revision, not undocumented edits.

## 21. References

- [RFC 9106](https://www.rfc-editor.org/rfc/rfc9106.html): Argon2id and parameters.
- [RFC 5869](https://www.rfc-editor.org/rfc/rfc5869.html): HKDF.
- [Libsodium XChaCha20-Poly1305](https://doc.libsodium.org/secret-key_cryptography/aead/chacha20-poly1305/xchacha20-poly1305_construction): AEAD primitive and nonce requirements; not an endorsement of this protocol.
- [Graft Remote Sync 1.0](./graft-remote-sync-1.0.md): inherited repository synchronization safety.
