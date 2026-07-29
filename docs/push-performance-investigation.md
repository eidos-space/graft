# Push performance investigation

Date: 2026-07-29

Status: DONE_WITH_REMAINING_PERFORMANCE_GAP — staging authorization is healthy,
the client and Worker changes are deployed/tested on staging, and the full
matrix has reproducible after-measurements. Reliability improved, but CLI and
resident SDK incremental HTTP pushes still miss the proposed latency budgets.

Scope: the Graft CLI, `@eidos.space/graft` `RepositorySession`, the public
HTTP Remote protocol, and the staging Sync Worker. All benchmarks used
disposable repositories and branches. No user-owned Space or production
resource was read, mutated, or benchmarked.

## Outcome

The slow incremental push is not primarily SQLite row-diff, hashing, local
object discovery, or payload size. A resident SDK session completes the entire
local equivalent of a no-op push in 1.4 ms median, a one-line push in 22.8 ms,
and a one-row SQLite push in 38.1 ms. On current staging from Shanghai, the
corresponding resident HTTP medians are 1.370 s, 3.855 s, and 5.228 s. The
one-row request body is only about 2.1 KiB median but requires six serialized
HTTP requests.

The dominant cause was compounded Remote request latency:

1. The original push constructed three independent HTTP connection pools:
   remote-head discovery, SQLite snapshot publication, and object/ref
   publication.
2. HTTP/1.1 requests within each phase were serialized. A one-row push used
   five top-level Remote requests in the observed trace.
3. Every top-level request repeated identity authorization and repository
   directory lookup in the Worker.
4. Every immutable write reserved quota, performed a conditional R2 write,
   read R2 metadata again, and committed or observed usage. A request without
   a content length additionally staged the body through a temporary R2
   multipart object.
5. From the Shanghai test path, a new TLS connection costs approximately
   0.62-0.90 s through the configured network path. A reused connection cost
   approximately 0.30-0.33 s per top-level request.

The first attempt to share a single long-lived HTTP/1 pool exposed a separate
reliability defect in this network path: a small PUT could hang when it reused a
read connection, and a resident session's next push could hang when it reused
the previous command's GET connection. The final client uses separate read and
mutation pools within one command, drains missing GET bodies, and starts fresh
pools at each top-level repository command. This removes the observed hangs
without changing protocol or publication ordering, but deliberately gives up
cross-push connection reuse on affected proxies.

Existing-history traversal is a separate contributor to first/new-branch
pushes and a possible contributor to the reported
15-second incident. Incremental pushes stop graph traversal at the known remote
head and do not enumerate all remote object IDs. First pushes do enumerate the
reachable local history. A deliberately reused disposable repository with
roughly 100 accumulated objects spent 19.67 s in object negotiation and took
25.04 s overall. A fresh current CLI first push took 12.54 s. The incident
magnitude is therefore consistent with request fan-out plus existing-history
work, but the original Space was intentionally not inspected.

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

The machine was Apple arm64 with the HTTP route originating in Shanghai. The
pre-change and local baselines used Node 24.18; the final staging matrix used
Node 26.0. Times are wall-clock seconds unless marked `ms`. Incremental cells
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

Staging identity was isolated onto the local `staging` branch, deployed as
version `e6011c7d-433d-44b3-b605-97333ff29a55`, and verified with the disposable
read/write smoke account. Sync Worker version
`57ee312a-dccf-4aa1-b00b-1edcd796505b` was then deployed and passed descriptor,
push, clone, content, commit, and usage verification. The following fresh-Remote
results were measured from Shanghai on 2026-07-29. Incremental cells are 10
runs; first push is one run.

| Mode | First | No-op p50/p95 | Text p50/p95 | SQLite row p50/p95 | 256 KiB p50/p95 |
| --- | ---: | ---: | ---: | ---: | ---: |
| CLI cold | 12.539 s | 1.745/2.292 s | 7.777/15.777 s | 6.954/9.896 s | 5.888/9.090 s |
| SDK cold | 7.580 s | 1.650/3.505 s | 4.277/4.697 s | 5.979/7.005 s | 5.633/6.063 s |
| SDK resident | 8.301 s | 1.370/1.492 s | 3.855/4.209 s | 5.228/8.100 s | 5.293/5.559 s |

The CLI comparison is intentionally not presented as a blanket improvement.
Versus the pre-change CLI run, first push improved 0.5% and the representative
change improved 28.5% at p50, but no-op p50 regressed 11.5%, text p50 regressed
63.1%, and SQLite-row p50 regressed 6.8%. The text CLI run included large Worker
and R2 outliers, with 4.007 s median and 12.967 s p95 summed Worker time. These
results show material staging variance and no proven small-change CLI speedup.

The current resident SDK path versus the old CLI incident baseline is more
favorable at p50: no-op is 12.5% faster, text 19.2% faster, SQLite row 19.7%
faster, and 256 KiB 35.7% faster. That is a cross-mode comparison, not a claim
that the HTTP protocol itself improved by those percentages. SQLite-row p95
regressed 14.4% because one run spent 3.000 s in authorization.

The checked-in harness can reproduce individual modes without losing completed
results when another mode fails:

```sh
GRAFT_PUSH_BENCH_HTTP_REMOTE='<disposable remote>' \
GRAFT_REMOTE_TOKEN='<staging token>' \
GRAFT_CLI_PATH=target/release/graft \
GRAFT_PUSH_BENCH_ITERATIONS=10 \
GRAFT_PUSH_BENCH_TARGETS=http \
GRAFT_PUSH_BENCH_MODES=sdk-warm \
pnpm --dir packages/graft-sdk bench:push
```

### Current request and timing attribution

The table uses resident SDK medians, which avoid CLI process noise. Request and
response bytes include only bodies reported by the safe trace, not HTTP/TLS
framing.

| Case | Requests | Request bytes | Push p50 | HTTP client sum | Worker total sum | Network/client-edge remainder |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| No-op | 1 | 0 B | 1.370 s | 1.363 s | 0.151 s | 1.212 s (88.5%) |
| Text | 4 | 1,506 B | 3.855 s | 3.837 s | 1.120 s | 2.717 s (70.5%) |
| SQLite row | 6 | 2,096 B | 5.228 s | 5.200 s | 2.155 s | 3.045 s (58.3%) |
| 256 KiB | 5 | 264,578 B | 5.293 s | 5.266 s | 1.584 s | 3.682 s (69.6%) |

For the median SQLite-row run, authorization sums to 0.500 s and repository
directory lookup to 0.075 s. The remaining approximately 1.580 s of Worker time
is chiefly Durable Object/R2 quota and object work. The 3.045 s client-minus-
Worker remainder is DNS/TCP/TLS/proxy/edge RTT, response delivery, and client
scheduling. This preserves the required separation: eidos.space authorization
is material but is not the majority of elapsed time; data-plane request fan-out
and R2/DO work plus geographic network latency dominate.

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

## Git-style receive-pack round

The next optimization follows Git smart HTTP's publish shape: advertise/read
the current ref, send the new pack, and update the ref as one receive operation.
Graft v1 now advertises an optional `receive-pack` capability. A supporting
Remote accepts the object pack and index in one length-delimited stream, writes
both immutable objects in order, and performs the ref compare-and-swap only
after both writes succeed. A client that receives a protocol-valid `404` or
`405` drains the response and falls back to the original pack PUT, index PUT,
and CAS sequence.

Only the staging Sync Worker was deployed for this round. Version
`2da2d1b4-c851-42db-8b7c-6fe010871021` passed an authenticated
provision/push/clone/content/usage round trip before measurement. The following
fresh-Remote matrix was measured from Shanghai on 2026-07-30. Incremental cells
are 10 runs; first push is one run.

| Mode | First | No-op p50/p95 | Text p50/p95 | SQLite row p50/p95 | 256 KiB p50/p95 |
| --- | ---: | ---: | ---: | ---: | ---: |
| CLI cold | 7.632 s | 1.728/2.344 s | 3.897/4.994 s | 6.002/8.203 s | 5.503/7.105 s |
| SDK cold | 7.377 s | 1.893/2.344 s | 4.053/5.293 s | 5.572/6.803 s | 5.738/7.901 s |
| SDK resident | 6.782 s | 1.250/2.392 s | 3.530/4.192 s | 5.107/7.426 s | 5.039/8.972 s |

Against the immediately preceding staging matrix, CLI text improved 49.9% at
p50 and 68.3% at p95. CLI SQLite-row improved 13.7%/17.1%, and the 256 KiB case
improved 6.5%/21.8%. CLI no-op, whose request path is unchanged, moved only
1.0% at p50 and regressed 2.3% at p95. Resident SDK text improved 8.4%/0.4%
and SQLite-row improved 2.3%/8.3%. Resident no-op and 256 KiB p95 regressed by
60.3% and 61.4% respectively despite improved p50, which is evidence of
staging/network variance rather than a claim of uniformly controlled tail
latency.

| Case | Requests before/after | Request bytes p50 | Push p50 | HTTP client sum | Worker total sum | Client-edge remainder |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| No-op | 1 / 1 | 0 B | 1.250 s | 1.244 s | 0.124 s | 1.120 s (90.0%) |
| Text | 4 / 2 | 1,441 B | 3.530 s | 3.512 s | 1.211 s | 2.301 s (65.5%) |
| SQLite row | 6 / 4 | 2,031 B | 5.107 s | 5.083 s | 2.249 s | 2.834 s (55.8%) |
| 256 KiB | 5 / 3 | 264,514 B | 5.039 s | 5.008 s | 1.672 s | 3.336 s (66.6%) |

The byte count is effectively unchanged; the measured win comes from removing
two serialized HTTP/auth/directory/Worker invocations. SQLite still performs a
segment PUT and storage-commit PUT before receive-pack, and external large-file
content still has its own PUT. Extending the streamed immutable-object bundle
to those object classes is the next Git-like request-fan-out reduction, but it
requires a generic framed manifest and must retain segment-before-commit and
ref-last publication semantics.

The post-change local `fs://` control remains within the existing budget. A
separate resident rerun was used after the full-matrix 256 KiB cell encountered
a local scheduling outlier.

| Mode | First | No-op p50/p95 | Text p50/p95 | SQLite row p50/p95 | 256 KiB p50/p95 |
| --- | ---: | ---: | ---: | ---: | ---: |
| CLI cold | 472.0 ms | 437.9/460.0 ms | 452.1/483.8 ms | 474.8/489.9 ms | 466.1/477.2 ms |
| SDK cold | 470.8 ms | 422.1/443.9 ms | 454.2/700.2 ms | 459.2/477.9 ms | 455.7/462.8 ms |
| SDK resident rerun | 33.7 ms | 1.5/1.8 ms | 23.5/27.6 ms | 37.5/40.3 ms | 37.9/39.2 ms |

## Git-style generic receive-bundle round

The second Git-style round extends the receive operation to every immutable
object needed by a normal single-branch push. The optional `receive-bundle`
request starts with a bounded JSON manifest and then streams SQLite segments,
SQLite storage commits, external file payloads, the object pack, and its index
as exact-length frames. The Remote writes frames in manifest order and performs
the ref compare-and-swap last. This preserves segment-before-storage-commit,
pack-before-index, and objects-before-ref publication without buffering the
whole request.

Old Remotes remain compatible: a protocol-valid `404` or `405` falls back to
individual immutable uploads plus `receive-pack`. A pre-existing object with
different bytes falls back to the original upload/read/equality check instead
of treating `412 Precondition Failed` as success. The manifest is capped at 16
KiB and 256 objects, and truncated or trailing bodies cannot publish the ref.

Only the staging Sync Worker was deployed. Version
`5ed2bc57-ae34-4a23-86ff-5d8c18318a82` passed an authenticated disposable
push/clone/content/usage round trip before measurement. The following
fresh-Remote matrix was measured from Shanghai on 2026-07-30 with the final
client. Incremental cells contain 10 runs; first push contains one run.

| Mode | First | No-op p50/p95 | Text p50/p95 | SQLite row p50/p95 | 256 KiB p50/p95 |
| --- | ---: | ---: | ---: | ---: | ---: |
| CLI cold | 6.347 s | 1.670/2.256 s | 3.719/4.199 s | 3.908/4.338 s | 4.266/4.964 s |
| SDK cold | 5.747 s | 1.641/3.456 s | 3.975/5.831 s | 3.936/4.258 s | 4.081/6.604 s |
| SDK resident | 5.307 s | 1.246/1.499 s | 3.576/7.103 s | 3.158/4.251 s | 3.828/4.829 s |

Against `receive-pack`, resident SQLite row improved from 5.107/7.426 s to
3.158/4.251 s, a 38.2% p50 and 42.8% p95 reduction. The 256 KiB case improved
24.0%/46.2%. CLI SQLite row improved 34.9%/47.1%, and CLI 256 KiB improved
22.5%/30.1%. Text keeps the same two-request shape and its resident p95
regressed because of staging variance, so this is not evidence of a uniform
text-change tail-latency improvement. The original approximately 15 s incident
and this fresh disposable CLI result are not controlled equivalents, but the
current 3.908/4.338 s result is about 74% below that observed duration.

| Case | Requests: original / receive-pack / receive-bundle | Request bytes p50 | Push p50 | HTTP client sum | Worker total sum | Client-edge remainder |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| No-op | 1 / 1 / 1 | 0 B | 1.246 s | 1.240 s | 0.133 s | 1.107 s (89.3%) |
| Text | 4 / 2 / 2 | 1,441 B | 3.576 s | 3.561 s | 1.337 s | 2.224 s (62.5%) |
| SQLite row | 6 / 4 / 2 | 2,235 B | 3.158 s | 3.139 s | 1.865 s | 1.274 s (40.6%) |
| 256 KiB | 5 / 3 / 2 | 264,665 B | 3.828 s | 3.795 s | 1.704 s | 2.091 s (55.1%) |

For the median resident SQLite row, authorization sums to 0.198 s and
repository directory lookup to 0.027 s. Approximately 1.640 s of Worker time is
therefore data-plane object/quota/R2/ref work; another 1.274 s is outside the
Worker. Batching removed two complete authenticated Worker invocations but did
not reduce the payload materially.

The bundle POST alone reuses the ref-read client's HTTP/1.1 connection. A
controlled 10-run resident comparison against the otherwise identical
split-pool build improved SQLite row from 3.997/5.334 s to 3.844/4.470 s
(3.8%/16.2%) and 256 KiB from 4.494/6.539 s to 3.659/5.926 s
(18.6%/9.4%), with no connection stalls. Legacy uploads and receive-pack retain
the isolated upload pool because broader reuse had previously stalled on the
available proxy path.

The final local `fs://` matrix shows that the additional manifest and framing
work did not move the bottleneck into the client:

| Mode | First | No-op p50/p95 | Text p50/p95 | SQLite row p50/p95 | 256 KiB p50/p95 |
| --- | ---: | ---: | ---: | ---: | ---: |
| CLI cold | 425.8 ms | 419.1/441.9 ms | 437.8/452.8 ms | 538.1/592.1 ms | 442.0/512.0 ms |
| SDK cold | 451.9 ms | 412.1/433.9 ms | 444.2/455.1 ms | 458.9/473.2 ms | 442.0/460.9 ms |
| SDK resident | 35.3 ms | 1.4/3.8 ms | 24.0/26.2 ms | 35.8/42.4 ms | 38.1/39.8 ms |

The resident SQLite client work is about 36 ms versus 3.158 s on staging. The
HTTP path therefore accounts for approximately 99% of the measured incremental
push latency on this network path.

## Implemented client changes

- `RemoteCredentials` now lazily shares dedicated read/control and
  upload/mutation `reqwest::Client` pools across every Remote built during one
  top-level command. Tokens stay on each Remote and can still be rotated; debug
  output exposes only whether a pool was initialized.
- Resident repository sessions reset both pools between commands. This retains
  intra-push connection reuse while avoiding the reproducible cross-command
  stale connection hang on the Shanghai proxy path. Missing GET response bodies
  are explicitly drained before a connection can return to the pool.
- Immutable upload streams now carry exact `Content-Length` without copying
  their content into memory. This avoids the Worker's unknown-length multipart
  staging path.
- SQLite snapshot segment and commit publication is serialized so a snapshot
  commit cannot be visible before its segment. General objectstore concurrency
  remains five; HTTP snapshot upload concurrency is one.
- The HTTP client retains HTTP/1.1 and uses DNS caching, a five-second connect
  timeout, and a 30-second total request timeout. HTTP/2 was tested, but the
  available network proxy produced stalls and high outliers, so it was not
  shipped.
- `GRAFT_PUSH_TRACE=1` emits newline-delimited, schema-versioned JSON for fixed
  phase names and HTTP operation names. It reports duration, status, byte
  counts, generated correlation ID, and only the `auth`, `directory`, and
  `total` server timing metrics. It never emits bearer data, headers, URLs,
  repository paths, user paths, or row/file contents.
- The repeatable benchmark covers CLI cold, SDK cold, and resident SDK modes;
  local and HTTP Remotes; first/no-op/text/SQLite-row/representative changes;
  and aggregates median/p95, requests, bytes, and client/server timing. It can
  select a target/mode, emits only safe worker progress, and reports the last
  case/run/operation on failure.
- HTTP publication uses the optional `receive-pack` capability when advertised:
  pack, index, and final ref CAS share one request body and one Worker
  invocation. Protocol-valid old Remotes continue to work through automatic
  `404`/`405` fallback.
- Normal single-branch publication uses `receive-bundle` when advertised:
  SQLite snapshot objects, external payloads, pack, index, and final ref CAS
  share one ordered streaming request. Old-Remote and immutable-collision
  fallbacks retain the existing verification behavior. Refspec and `--all`
  publication continue through the established path.
- The bundle POST reuses the ref-read connection. Other mutation requests stay
  on the isolated upload pool, limiting the change to the controlled path that
  completed the 10-run staging experiment without stalls.

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
- Receive-pack splits the request stream into bounded pack and index
  substreams. Cloudflare `FixedLengthStream` preserves direct streaming into R2;
  truncated or trailing bodies abort before ref publication, existing immutable
  objects are idempotent, and quota accounting uses each object's declared
  length rather than the combined request length.
- Receive-bundle applies the same direct-streaming and ref-last rules to every
  manifest object. Quota accounting is performed per frame, including retries
  of existing objects, rather than charging the combined request body.

The public Remote protocol remains backward compatible through an optional v1
capability. Immutable writes still use create-only semantics; packs still
publish before indexes; refs still publish last using a compare-and-swap in the
repository Durable Object.

## Staging architecture and rollback

Staging is `eidos-graft-remote-staging` with:

- R2 binding `GRAFT_OBJECTS` to `eidos-graft-remote-staging-objects`;
- service binding `EIDOS_ACCOUNT` to `eidos-space-staging`;
- SQLite Durable Objects for repository refs, repository directory, and usage;
- enforced Sync entitlement and quota checks;
- full invocation logs and 1% distributed trace sampling.

No production deployment, production data, package publication, new R2 bucket,
or additional persistent environment was created. The short-lived geographic
probe left no Worker resource. Repeated fresh-Remote retries created
`perf-final-*` and `perf-diag-*` staging repositories containing only synthetic
benchmark data. The management API currently has no repository-delete endpoint,
so their tens of MiB of R2/metadata data could not be removed safely. The cost
is small but nonzero; adding an authenticated staging cleanup API is a follow-up.

Current staging Sync deployment: `5ed2bc57-ae34-4a23-86ff-5d8c18318a82`.
Current staging identity deployment:
`e6011c7d-433d-44b3-b605-97333ff29a55`.
The immediately prior Sync deployment is the receive-pack build
`2da2d1b4-c851-42db-8b7c-6fe010871021`; its predecessor is
`57ee312a-dccf-4aa1-b00b-1edcd796505b`.
The safe-timing deployment before error-log hardening is
`922a7ab3-bc2d-4650-bce6-6d43312512be`. The object-write-only optimized deployment is
`f2c958bf-d10f-4960-9d7d-2d1aabbd1d6e`. The full pre-change Sync deployment is
`1f43e133-fb88-4168-a86c-13b058ab1033`.

Rollback only the Sync Worker, from the Eidos repository:

```sh
pnpm --dir apps/graft-remote exec wrangler rollback \
  2da2d1b4-c851-42db-8b7c-6fe010871021 \
  --env staging -m 'Rollback receive-bundle staging deployment'
```

Use `f2c958bf-d10f-4960-9d7d-2d1aabbd1d6e` to remove timing and log hardening
while retaining the object-write optimization. Use the pre-change deployment
ID to remove every Sync optimization round.

If the identity staging changes must also be rolled back, run from the
eidos.space repository only after Sync has been rolled back or verified against
the older identity contract:

```sh
pnpm exec wrangler rollback \
  24a9a0d5-2468-40b5-9cf6-b7b4c517035b \
  --env staging -m 'Rollback hosted Sync identity changes'
```

## Correctness gates

The following passed after the changes:

- `just test`: 590 repository tests, 81 vendor doc tests, one Graft doc test,
  and all SQL integration scripts on the integrated topic branch;
- final focused suites: 201 Graft core tests plus its doc test and 73
  `graft-sqlite` library tests;
- 17 focused HTTP Remote tests, including within-command reuse, command-boundary
  pool reset, read/upload pool isolation, missing-body drain, exact content
  length, early `412` without fully reading the body, CAS behavior, credential
  redaction, and `Server-Timing` filtering;
- push/snapshot tests and crash-after-remote-commit recovery;
- clone, fetch, pull, multi-file/workspace, force-push, non-fast-forward,
  snapshot integrity, and SQLite row-diff/merge tests in the full suite;
- eight native `RepositorySession` SDK tests;
- 16 staging Worker tests plus TypeScript checking;
- all ten core protocol, one Hono adapter, and nine Cloudflare adapter tests;
- staging smoke verification and a real authenticated CLI provision/push/clone/
  content/commit/usage round trip;
- the final 10-run resident staging matrix, including recovery from the
  previously reproducible cross-command timeout.
- receive-pack request framing, exact concatenated bodies, old-Remote fallback,
  duplicate immutable retry, quota delta, truncated-body ref safety, and direct
  R2 streaming tests.
- receive-bundle framing and limits, ordered object publication, collision and
  old-Remote fallback, exact concatenated bodies, connection reuse, per-object
  quota accounting, retry idempotence, truncated/trailing-body ref safety, and
  direct R2 streaming tests.

`cargo fmt --all -- --check`, `cargo check --workspace --all-targets`, and
targeted Clippy with `--no-deps` pass. Whole-workspace Clippy remains blocked by
pre-existing `deny(clippy::cargo)` failures and other lints in vendored `fjall`,
including missing package metadata across existing workspace crates.

## Budgets and remaining work

Proposed budgets and current disposition:

| Path                                | No-op p50/p95 | Incremental text/row p50/p95 |
| ----------------------------------- | ------------: | ---------------------------: |
| Resident SDK, local                 |       5/10 ms |                     50/75 ms |
| CLI, local                          |    500/650 ms |                   550/750 ms |
| Resident SDK, staging, warm session |   0.75/1.25 s |                    2.0/3.0 s |
| CLI, staging, cold process          |    1.75/2.5 s |                    3.0/4.5 s |

Local paths meet budget. Staging resident no-op and incremental text/SQLite
pushes still miss their p50/p95 budgets. Cold CLI incremental p50 also misses,
although the final text, SQLite, and 256 KiB p95 values are within the 4.5 s
tail budget. These budgets should remain targets rather than being raised to
match the current implementation.

Remaining risks and follow-ups:

- The interrupted-upload/ref-failure recovery gates passed in the repository
  suite, but a fault-injected authenticated staging run is still recommended;
  production-equivalent fault injection was not added to the public service.
- Geographic latency is material in the isolated two-path probe. The full
  authenticated push matrix was measured only from Shanghai; run it from a
  second authenticated client region before changing storage placement.
- The optional receive-pack extension removes two serialized publications, but
  SQLite and external payloads remain outside its object bundle. If
  warm-session p95 misses budget after these changes, as it does here, the next
  measured optimization should generalize the framed immutable-object bundle
  with fallback to base protocol v1, not weaken CAS or publication order.
- Short-lived server-side authorization or directory caching could save tens
  to hundreds of milliseconds per push, but entitlement revocation and
  repository deletion semantics must be specified before adding it. It will not
  remove the multi-second geographic/request-fan-out remainder by itself.
- Staging Worker/R2 variance is high enough that p95 is not controlled. Add
  per-R2-operation `Server-Timing` or internal sampled telemetry, then evaluate
  batched immutable existence/write operations and parallel work only where
  pack-before-index and segment-before-commit ordering remain explicit.
