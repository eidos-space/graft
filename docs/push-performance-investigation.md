# Push performance investigation

Date: 2026-07-29

Status: DONE_WITH_CONCERNS — implementation and local correctness/performance
work are complete; deployed after-measurements are blocked by the independent
staging identity outage described below.

Scope: the Graft CLI, `@eidos.space/graft` `RepositorySession`, the public
HTTP Remote protocol, and the staging Sync Worker. All benchmarks used
disposable repositories and branches. No user-owned Space or production
resource was read, mutated, or benchmarked.

## Outcome

The slow incremental push is not primarily SQLite row-diff, hashing, or local
object discovery. A resident SDK session completes the entire local equivalent
of a no-op push in 1.4 ms median, a one-line push in 22.8 ms, and a one-row
SQLite push in 38.1 ms. The pre-change staging medians from Shanghai were
1.565 s, 4.769 s, and 6.512 s respectively.

The dominant cause was compounded Remote request latency:

1. The push constructed three independent HTTP connection pools: remote-head
   discovery, SQLite snapshot publication, and object/ref publication.
2. HTTP/1.1 requests within each phase were serialized. A one-row push used
   five top-level Remote requests in the observed trace.
3. Every top-level request repeated identity authorization and repository
   directory lookup in the Worker.
4. Every immutable write reserved quota, performed a conditional R2 write,
   read R2 metadata again, and committed or observed usage. A request without
   a content length additionally staged the body through a temporary R2
   multipart object.
5. From the Shanghai test path, a new TLS connection cost approximately
   0.62-0.90 s through the configured network path. A reused connection cost
   approximately 0.30-0.33 s per top-level request.

Existing-history traversal is a separate possible contributor to the reported
15-second incident. Incremental pushes stop graph traversal at the known remote
head and do not enumerate all remote object IDs. First pushes do enumerate the
reachable local history. The disposable reproduction reached 12.60 s for a
first push and 8.23 s for the representative larger incremental case, so the
incident magnitude is consistent with request fan-out plus first/history work,
but the original Space was intentionally not inspected.

## Phase attribution

One representative pre-change one-row staging push took approximately 6.7 s.
Cloudflare request-tail timings and client measurements separated that time as
follows:

| Phase                            | Requests | Worker wall time | Client-observed time | Attribution                                               |
| -------------------------------- | -------: | ---------------: | -------------------: | --------------------------------------------------------- |
| Remote ref read                  |        1 |            63 ms |       included below | authorization, directory, ref DO, network                 |
| SQLite snapshot commit PUT       |        1 |           991 ms |       included below | authorization, directory, quota DO, R2                    |
| Object pack PUT                  |        1 |           661 ms |       included below | authorization, directory, quota DO, R2                    |
| Object index PUT                 |        1 |           626 ms |       included below | authorization, directory, quota DO, R2                    |
| Ref compare-and-swap             |        1 |           132 ms |       included below | authorization, directory, repository DO                   |
| Total Worker wall-time sum       |        5 |          2.473 s |           6.7 s push | server-side work                                          |
| Client/edge/connection remainder |        5 |              n/a |          about 4.2 s | TLS, proxy/edge RTT, response delivery, client scheduling |

Within the five Worker invocations, repository directory calls summed to 45 ms,
four quota reservations summed to 168 ms, and four quota commit/observe calls
summed to 147 ms. The remainder was primarily R2 operations and per-request
authorization. A warm direct identity request from the same client path took
approximately 0.28-0.29 s end to end, while Worker-side directory/repository
operations were generally 0.02-0.04 s. This makes the distinction important:
the authorization/control plane is repeated work, but client-to-edge request
fan-out and immutable-object storage dominate the observed elapsed time.

Local phase traces corroborate the attribution:

| Current resident SDK phase |   No-op |    One-row SQLite | 256 KiB representative |
| -------------------------- | ------: | ----------------: | ---------------------: |
| Entire push                | 1.04 ms |          27.57 ms |               23.38 ms |
| Remote head read           | 0.25 ms | included in total |      included in total |
| Snapshot publication       | 0.05 ms |           9.81 ms |             0.1-0.2 ms |
| Snapshot collect/build     |     n/a |           0.12 ms |                    n/a |
| Snapshot local upload      |     n/a |           9.06 ms |                    n/a |
| Object discovery/hash      |     n/a |           0.22 ms |                0.32 ms |
| Large-file processing      |     n/a |               n/a |                4.56 ms |
| Pack creation              |     n/a |           8.34 ms |                8.09 ms |
| Object/ref publication     | 0.39 ms |          17.27 ms |               22.72 ms |

## Benchmark matrix

The machine was Apple arm64, Node 24.18, with the HTTP route originating in
Shanghai. Times are wall-clock seconds unless marked `ms`. Incremental cells
use 10 runs and report median/p95. First-push cells are single diagnostic runs.

### Pre-change CLI baseline

| Target       | First push | No-op p50/p95 |  Text p50/p95 | SQLite row p50/p95 | Representative change |
| ------------ | ---------: | ------------: | ------------: | -----------------: | --------------------: |
| `fs://`      |     0.45 s | 0.418/0.522 s | 0.438/0.526 s |      0.468/0.501 s |               0.467 s |
| staging HTTP |    12.60 s | 1.565/2.592 s | 4.769/5.477 s |      6.512/7.078 s |               8.232 s |

The pre-change manual harness did not yet emit safe byte/request counters;
request counts above were obtained from the correlated Worker tail. The
checked-in benchmark now records count, request bytes, response bytes, summed
client HTTP time, and the whitelisted `Server-Timing` metrics for every run.

### Current local baseline and SDK regression gate

| Mode         |    First |  No-op p50/p95 |   Text p50/p95 | SQLite row p50/p95 | 256 KiB p50/p95 |
| ------------ | -------: | -------------: | -------------: | -----------------: | --------------: |
| CLI cold     | 420.2 ms | 429.7/485.2 ms | 442.3/475.5 ms |     421.3/479.0 ms |  458.9/479.2 ms |
| SDK cold     | 462.9 ms | 400.0/442.7 ms | 441.9/458.9 ms |     445.9/498.0 ms |  441.9/454.8 ms |
| SDK resident |  34.6 ms |     1.4/3.5 ms |   22.8/24.7 ms |       38.1/39.9 ms |    37.6/39.1 ms |

Cold local results are dominated by process/native-module startup. The
resident results show that reusing an SDK `RepositorySession` remains the
intended low-latency path.

### Post-change staging

The exact post-change staging matrix is pending restoration of the separate
staging identity service. At measurement time its userinfo route returned 404,
causing both the optimized Sync Worker and a controlled rollback to the prior
Sync Worker to return 503. Safe timing responses attribute only 3-13 ms to
Worker-side authorization before failure; the client still observes roughly
0.9-1.4 s, independently confirming the network/edge component.

The account/identity repository contains the expected userinfo implementation
only as part of a large, unrelated dirty worktree. Current deployed account
version `24a9a0d5-2468-40b5-9cf6-b7b4c517035b` still returns 404. Deploying
that dirty tree or rolling the whole account service back would violate the
isolation rule and could disrupt concurrent staging work, so neither action was
taken.

Do not use the pre-change values as a claimed post-change improvement. Run the
checked-in harness once staging authorization is healthy and append its JSON
summary here:

```sh
GRAFT_PUSH_BENCH_HTTP_REMOTE='<disposable remote>' \
GRAFT_REMOTE_TOKEN='<staging token>' \
GRAFT_CLI_PATH=target/release/graft \
GRAFT_PUSH_BENCH_ITERATIONS=10 \
pnpm --dir packages/graft-sdk bench:push
```

### Geographic control

A 10-request, connection-warm health probe was also run from two paths. This is
not a substitute for the authenticated push matrix; it intentionally removes
authorization, Durable Objects, and R2 to isolate client-to-edge geography.

| Origin path | Remote execution location | p50 | p95 |
| --- | --- | ---: | ---: |
| Shanghai client through the configured network path | client process | 222 ms | 928 ms |
| Disposable Cloudflare remote-dev probe | `AUS` POP | 8 ms | 31 ms |

The approximately 28x median difference confirms that geography/network path
is material when the protocol performs several serialized requests. The
remote-dev probe was stopped, its temporary source was deleted, and the
Cloudflare API confirmed that no persistent probe Worker exists.

## Implemented client changes

- `RemoteCredentials` now lazily shares one `reqwest::Client` across all Remote
  instances derived from the same repository/session credentials. Tokens stay
  on each Remote and can still be rotated; debug output exposes only whether a
  client was initialized.
- Immutable upload streams now carry exact `Content-Length` without copying
  their content into memory. This avoids the Worker's unknown-length multipart
  staging path.
- The HTTP client retains HTTP/1.1 and uses DNS caching plus a bounded connect
  timeout. HTTP/2 was tested, but the available network proxy produced stalls
  and high outliers, so it was not shipped.
- `GRAFT_PUSH_TRACE=1` emits newline-delimited, schema-versioned JSON for fixed
  phase names and HTTP operation names. It reports duration, status, byte
  counts, generated correlation ID, and only the `auth`, `directory`, and
  `total` server timing metrics. It never emits bearer data, headers, URLs,
  repository paths, user paths, or row/file contents.
- The repeatable benchmark covers CLI cold, SDK cold, and resident SDK modes;
  local and HTTP Remotes; first/no-op/text/SQLite-row/representative changes;
  and aggregates median/p95, requests, bytes, and client/server timing.

## Implemented staging changes

- A quota record that already tracks an immutable path now performs one object
  HEAD and returns `412 Precondition Failed` immediately when the object is
  present. The incoming body is explicitly canceled, avoiding a second R2
  conditional write and preventing an unread streamed body from stalling the
  HTTP connection.
- A newly created known-length object commits the already-known byte count and
  no longer performs a redundant post-write R2 HEAD.
- Known-length collisions cancel the incoming stream before returning. The
  unknown-length crash-cleanup path remains intact.
- Responses echo only validated/generated correlation IDs and return safe
  `Server-Timing` values for authorization, directory resolution, and total
  Worker time.
- Unexpected failures are logged by fixed operation class. Raw error text,
  repository/object paths, request URLs, bearer data, and user identifiers are
  excluded in both the protocol package and the staging application.

The public Remote protocol is unchanged. Immutable writes still use create-only
semantics; packs still publish before indexes; refs still publish last using a
compare-and-swap in the repository Durable Object.

## Staging architecture and rollback

Staging is `eidos-graft-remote-staging` with:

- R2 binding `GRAFT_OBJECTS` to `eidos-graft-remote-staging-objects`;
- service binding `EIDOS_ACCOUNT` to `eidos-space-staging`;
- SQLite Durable Objects for repository refs, repository directory, and usage;
- enforced Sync entitlement and quota checks;
- full invocation logs and 1% distributed trace sampling.

No production deployment, production data, package publication, new R2 bucket,
or additional persistent environment was created. The short-lived geographic
probe left no Worker resource. The benchmark's immutable objects are disposable
staging-only data and are expected to remain below 10 MiB for a full three-mode
matrix.

Current staging Sync deployment: `7c2003fb-f781-4cdb-837c-475440b51d26`.
The safe-timing deployment before error-log hardening is
`922a7ab3-bc2d-4650-bce6-6d43312512be`. The object-write-only optimized deployment is
`f2c958bf-d10f-4960-9d7d-2d1aabbd1d6e`. The full pre-change Sync deployment is
`1f43e133-fb88-4168-a86c-13b058ab1033`.

Rollback only the Sync Worker, from the Eidos repository:

```sh
pnpm --dir apps/graft-remote exec wrangler rollback \
  922a7ab3-bc2d-4650-bce6-6d43312512be \
  --env staging -m 'Rollback failure-log hardening'
```

Use `f2c958bf-d10f-4960-9d7d-2d1aabbd1d6e` to remove timing and log hardening
while retaining the object-write optimization. Use the pre-change deployment
ID to remove every optimization round. Do not roll back `eidos-space-staging`
as part of this change: its current userinfo outage came from a separate,
concurrent staging deployment and was reproduced against both old and new Sync
Worker versions.

## Correctness gates

The following passed after the changes:

- `just test`: 590 repository tests, 81 vendor doc tests, one Graft doc test,
  and all SQL integration scripts;
- 14 focused HTTP Remote tests, including shared connection reuse, exact
  content length, early `412` without fully reading the body, CAS behavior,
  credential redaction, and `Server-Timing` filtering;
- push/snapshot tests and crash-after-remote-commit recovery;
- clone, fetch, pull, multi-file/workspace, force-push, non-fast-forward,
  snapshot integrity, and SQLite row-diff/merge tests in the full suite;
- eight native `RepositorySession` SDK tests;
- 14 staging Worker tests plus TypeScript checking;
- all seven core protocol, one Hono adapter, and nine Cloudflare adapter tests.

`cargo fmt --all -- --check`, `cargo check --workspace --all-targets`, and
targeted Clippy with `--no-deps` pass. Whole-workspace Clippy remains blocked by
pre-existing `deny(clippy::cargo)` failures and other lints in vendored `fjall`,
including missing package metadata across existing workspace crates.

## Budgets and remaining work

Proposed budgets, to be ratified by the post-change staging matrix:

| Path                                | No-op p50/p95 | Incremental text/row p50/p95 |
| ----------------------------------- | ------------: | ---------------------------: |
| Resident SDK, local                 |       5/10 ms |                     50/75 ms |
| CLI, local                          |    500/650 ms |                   550/750 ms |
| Resident SDK, staging, warm session |   0.75/1.25 s |                    2.0/3.0 s |
| CLI, staging, cold process          |    1.75/2.5 s |                    3.0/4.5 s |

Remaining risks and follow-ups:

- Staging after-numbers and the interrupted-upload retry must be rerun after
  the independent identity route is restored. Until then the performance
  outcome is evidence-backed locally and structurally, but not yet proven end
  to end on the deployed service.
- Geographic latency is material in the isolated probe. Run the full
  authenticated matrix from a second client region after identity recovery
  before changing storage placement.
- The public protocol still requires several serialized publications for an
  incremental change. If warm-session p95 misses budget after these changes,
  the next measured optimization should be an optional batch negotiation/write
  extension with fallback to protocol v1, not weaker CAS or publication order.
- Short-lived server-side authorization or directory caching could save tens
  of milliseconds per request, but entitlement revocation and repository
  deletion semantics must be specified before adding it.
