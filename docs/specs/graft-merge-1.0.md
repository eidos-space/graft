# Graft Merge 1.0

Status: Draft implementation-aligned normative baseline
Version: 1.0
Published: 2026-08-11
Canonical language: English

## Abstract

This specification defines Graft merge planning and application, fast-forward
and three-way topology, path and SQLite row conflicts, durable merge state,
version inspection, resolution operations, stale tokens, continuation, abort,
and recovery after process or session restart.

The central safety rule is that planning is read-only and application never
silently overwrites two divergent sides. Every unresolved path retains the
available base/ours/theirs versions and a durable recovery route.

## 1. Scope and terminology

`ours` is the local `HEAD` when a merge is planned. `theirs` is the resolved
target commit. `base` is their selected merge base. `result` is the current
staged or candidate path after automatic or manual resolution.

The repository object/index model is defined by [Graft Repository
1.0](./graft-repository-1.0.md), logical inspection by [Graft Diff
1.0](./graft-diff-1.0.md), and any ordinary-file replacement by [Graft
Worktree Materialization 1.0](./graft-worktree-materialization-1.0.md).

## 2. Merge state machine

```text
idle
  |
  | plan (read-only)
  v
plan: up_to_date | fast_forward | three_way
  |
  | apply with matching plan token / HEAD
  +----------------------+-----------------------+
  |                      |                       |
  v                      v                       v
unchanged            fast-forward       merging (durable)
                                             |
                         resolve paths / rows / cells / text
                                             |
                                  +----------+----------+
                                  |                     |
                               continue               abort
                                  |                     |
                                  v                     v
                          two-parent commit       original HEAD/index
```

Only one merge may be active in a repository. Applying a candidate while merge
state already exists MUST fail. Planning during an active merge MAY return
status/diagnostics but MUST NOT replace that merge.

## 3. Planning

### 3.1 Read-only contract

`planMerge`/`plan_merge_revision` resolves target and graph topology and
computes the necessary path/SQLite comparison. Retained SDK and browser API
profiles return a plan token; the one-shot human CLI may plan and apply under
one command without exposing that token. Planning MAY
hydrate immutable objects and snapshot pages, but MUST NOT move refs, change
the index, write merge records, resolve paths, or materialize the worktree.

### 3.2 Topology outcomes

| Outcome | Condition | Planned effect |
| --- | --- | --- |
| `up_to_date` | target is an ancestor of ours, including equality | no ref/index/worktree change |
| `fast_forward` | ours is an ancestor of target | move current branch/HEAD to target and project target as required |
| `three_way` | both sides diverged from a merge base | stage clean results and preserve conflicts in durable merge state |

An unborn local branch may fast-forward to the target when allowed by the
operation. When no common ancestor exists, the current implementation returns
`merge_base = null` and performs the three-way path comparison against an empty
base. Consequently, same-path additions from both histories are add/add
conflicts unless identical. Clients MUST display the absent base explicitly;
they MUST NOT invent a common ancestor.

When several incomparable best merge bases exist, the implementation selects
one deterministically: minimize the greater distance to either head, then the
sum of distances, then object ID. Version 1.0 exposes that selected base rather
than synthesizing a recursive merge base.

### 3.3 Plan token

The current plan token is the BLAKE3 digest of the exact serialized immutable
merge plan plus the effective policy token/version, including revision/target,
expected topology, checkout actions, candidate index, and outcome. It is opaque to clients and is not a repository
credential or globally unique nonce. Apply recomputes the plan against current
state and compares the digest. A malformed, mismatched, or stale token MUST
return a stale/invalid error and MUST make no merge-state change. Reapplying an
unchanged up-to-date no-op may remain valid; state-changing outcomes naturally
invalidate the old plan through changed state or active merge records.

API clients MUST treat plans as short-lived optimistic observations: after
stale failure they obtain fresh status and plan again. A one-shot CLI apply
still validates the expected `HEAD` inside the core before mutation.

## 4. Path-level three-way merge

For each canonical path, compare base (`B`), ours (`O`), and theirs (`T`):

| Relationship | Result |
| --- | --- |
| `O == T` | that version |
| `O == B` and `T != B` | theirs |
| `T == B` and `O != B` | ours |
| only one side adds a path | added side |
| both sides add identical content | shared addition |
| one side deletes and the other is unchanged | deletion |
| divergent modify/modify, add/add, or modify/delete | conflict unless a type-specific merge resolves it |

Clean results become Normal index entries or staged deletions. Conflicts retain
available Base/Ours/Theirs stages. Missing stage means that side deleted or
never contained the path. No algorithm may resolve a divergent path merely by
choosing the later timestamp.

Changing path kind or storage class across sides is a path conflict unless a
documented type-specific rule establishes an unambiguous result.

## 5. SQLite three-way merge

### 5.1 Eligibility

Row-level merge is attempted only when base, ours, and theirs are compatible
SQLite snapshots and the logical diff engine can safely identify the relevant
rows/schema/opaque surfaces. Missing, added/deleted, malformed, unsupported, or
incompatible database versions fall back to a whole-path conflict unless an
explicit clean path rule applies.

### 5.2 Row changes

Independent row changes merge. Identical changes on both sides collapse to one.
Conflicting changes to the same row identity produce a row conflict, including
incompatible update/update, update/delete, and delete/update pairs. Same-row
field merging is disabled by default. With the versioned policy's
`same_row_merge` flag enabled, update/update may combine columns using Base:
disjoint changed-column sets combine, equal values for a column collapse, and
different values for the same column produce a structured cell conflict.
Delete/update remains a conflict.

Two independently inserted rowid rows that collide MAY be merged by omitting
theirs' requested rowid and allowing SQLite to allocate a fresh one only when
both changes are inserts, the table has no `INTEGER PRIMARY KEY` rowid alias,
both semantic keys are known, and those semantic keys differ. It MUST NOT remap
a declared primary key or hide referential/opaque uncertainty.

### 5.3 Semantic keys

Semantic keys detect result-level collisions whose storage row identities
differ, including insert/insert, insert/update, and update/update. Selection order is a table-specific
`merge.semantic_keys.<table>` override, a resolvable
`merge.default_semantic_keys` list, then the first parsed declared primary-key
or unique constraint that is not merely the rowid alias. `BINARY` is the
default comparison; a configured `NOCASE` comparison follows SQLite's built-in
ASCII case folding. A collision becomes a semantic-key conflict and MUST NOT be
auto-remapped. It reports the two physical identities, display semantic key,
and applied collation without assigning business meaning.

Semantic-key conflicts currently require a whole-file/manual result; per-row
ours/theirs selection is not sufficient because both physical identities may
need coordinated editing.

### 5.4 Schema changes

Schema changes merge only through a supported deterministic resolver. The
current built-in schema resolver supports compatible `add_column` through
`alter_table_add_column`. Divergent definitions, deletion versus modification,
and unsupported operations remain schema conflicts.

### 5.5 Opaque/internal changes

Opaque SQLite structures never disappear silently. The default internal
resolver map is:

| Subject | Resolver |
| --- | --- |
| `index_btree` | `reindex` |
| `sqlite_sequence` | `sequence_max` |
| `sqlite_stat1` ... `sqlite_stat4` | `rebuild` |

Unknown, disabled, or incompatible opaque changes remain conflicts. Resolver
names are validated repository configuration, not arbitrary SQL hooks.

### 5.6 Candidate database construction

The engine builds every SQLite candidate in an isolated temporary database. It
applies supported row/schema/internal resolutions with foreign-key and trigger
side effects disabled for controlled replay, excludes configured/generated
columns from writes, then validates the candidate with SQLite integrity and
foreign-key checks before accepting it.

A candidate that fails validation MUST NOT replace the staged result or
worktree or change the prior merge journal. Temporary databases are cleaned on
success and failure. Directory repository sessions MUST plan every conflicted
SQLite path; each unmerged path exposes structured conflicts, a validation or
analysis error, a limitation, or an explicit executable action.

### 5.7 Managed columns

The finite managed-column resolver vocabulary is `ignore_for_conflict`, `max`,
`min`, `max_timestamp`, and `recompute`. Resolvers never execute application
code. `recompute` omits the column from replay, materializes the otherwise
merged candidate, and leaves the path unresolved with a recomputation contract.
After application-owned recomputation and validation, `stageMergeSqliteResult`
captures the physical database, re-runs integrity and foreign-key checks on the
exact private snapshot, then stages it. The legacy `generated_columns` setting
normalizes to `recompute`.

### 5.8 Versioned policy and CAS

The typed SDK exposes `getMergePolicy`, `validateMergePolicy`, and
`setMergePolicy`. Policy version 1 contains only the finite resolver vocabulary.
Every effective policy has a stable `policy_token`; setting policy requires the
previous token. `planToken` binds the target plan, policy token, and policy
version. A three-way apply freezes the effective policy in the durable merge
journal; changing it requires completing or aborting the merge and replanning.
Planning, status, and policy results report the actual token and version.

## 6. Durable merge state

A three-way apply writes enough state to reconstruct the merge after all
processes and SDK sessions close:

- original local commit in `ORIG_HEAD`;
- target commit in `MERGE_HEAD`;
- Base/Ours/Theirs/Normal index stages;
- a `merge-resolution-session.json` journal containing the original conflict
  stages, frozen policy/token/version, whole-path resolution, SQLite row/cell
  selections, and per-path analysis state where applicable;
- all status/index/resolution inputs from which the merge state token is
  deterministically reconstructed.

The current state token is prefixed `graft-merge-v1:` and hashes the complete
repository status, index, and optional `merge-resolution-session.json`. It is
stable across close/reopen when state is unchanged and changes after an index,
status, path-resolution, row-selection, or resolve-undo transition. The journal
MUST remain until successful `continueMerge` or `abortMerge`.

The physical worktree is not the sole record of a merge. Reopening the
repository MUST recover the same unresolved path/conflict count and available
versions from durable repository state. Missing or inconsistent merge records
MUST be reported as recoverable corruption; implementations MUST NOT assume the
worktree's current bytes are the intended result.

Application semantic providers use a separate `.graft/semantic-merge`
workspace. It is not part of the merge-state-token hash and MUST NOT itself
resolve a path. Each workspace manifest is instead bound to the exact active
state token, frozen policy token/version, Base/Ours/Theirs revisions, provider
identifier, and repository path. The workspace and any recorded domain
conflicts survive SDK close/reopen and remain until the merge successfully
continues or aborts.

## 7. Status and bounded conflict inspection

`getMergeStatus` reports at least active/inactive state, ours/theirs/base when
available, current merge token/state identity, unresolved counts, and whether
continuation is allowed.

Path and conflict listing APIs use deterministic path order and bounded pages.
Current SDK limits are 500 merge paths and 1000 conflict records per request.
Each conflict identifies its path, conflict category, available versions, and
row/schema/opaque detail needed to choose a supported resolution. Truncation or
pagination MUST be explicit.

During an active merge, conflict inspection MUST retain the original conflict
records for paths that have already collapsed to Normal/staged results. Their
current `status` and `resolution` MUST be reported so a client can reopen and
render the resolved SQLite tables without reconstructing them from worktree
bytes.

`readMergeVersion(path, version)` returns exact `base`, `ours`, `theirs`, or
current `result` content/snapshot. An unavailable side is reported as absent,
not substituted. Version reads are read-only.

`resolveMergeCell` selects ours or theirs for one structured cell conflict. It
uses the same state-token CAS and journal as row/table resolution; selection is
durable across reopen and is cleared by `unresolveMergePath` or abort.

`diffMergeSqlite(path, from, to, response)` compares two distinct immutable
`base`, `ours`, or `theirs` revisions during the active merge. It MUST remain
available while the index contains unresolved conflicts, MUST use the active
merge state token, MUST be cancellable and bounded, and MUST NOT write the
worktree, index, refs, or merge journal. Its generic result includes supported
table/row facts, schema entries (`name`, `entry_type`, `op`, new SQL, and old
SQL where present), opaque changes, analysis limitations, and explicit logical
status. Clients form a three-way view by comparing Base to each side.

If one side is physically different from Base but this analysis reports no
supported logical change, schema or row conflict, opaque change, or limitation,
the engine MAY resolve the file-level conflict to the other side without
applying SQL. For the common case where Theirs is logically equivalent to Base,
the safe result is Ours: this preserves every supported non-conflicting local
change and avoids an arbitrary whole-file choice. An existing active merge that
predates automatic resolution MUST expose this as an auto-resolvable conflict
with `recommended_result = ours`. Any opaque change or analysis limitation
prohibits this equivalence rule.

## 8. Resolution operations

Every mutating resolution MUST operate on current merge state. Retained SDK and
browser `merge-api` calls MUST carry the current merge-state token; a stale
token MUST leave all paths and selections unchanged. The one-shot human CLI
does not expose a client-held token: it acquires repository coordination,
reloads current state, and executes the requested resolution immediately.

### 8.1 Whole-path ours/theirs

`setMergePathResult` selects `ours` or `theirs` for the complete path. Selection
supports ordinary files, SQLite snapshots, and deletion. It replaces conflict
stages with one Normal result or a staged deletion. It MUST preserve the
unselected version in durable history/merge metadata until the merge completes
or aborts; it may not silently discard recovery access mid-operation.

### 8.2 Text result

`writeAndStageTextResult` accepts explicit UTF-8 result text for an eligible
ordinary text path, writes/stages that exact result, and resolves the path.
Invalid UTF-8 contract input, binary paths, path/token mismatch, or write
failure MUST leave the previous conflict state recoverable.

### 8.3 Row ours/theirs

`resolveMergeRow` selects ours or theirs for one eligible row-identity conflict.
Selections persist against the current merge identity. The engine rebuilds or
updates the candidate and validates it. Once every row conflict for a path is
resolved and no schema/opaque conflict remains, the path collapses to a Normal
SQLite result.

Per-row selection is unavailable for semantic-key, schema, opaque, malformed,
or unsupported conflicts. Clients MUST be told to use a whole-path/manual
result rather than receiving a false successful row resolution.

### 8.4 Table ours/theirs

`resolveMergeTable` selects one side for every independently row-resolvable
conflict in one SQLite table. It MUST preserve non-conflicting changes from
both sides, build and validate one candidate, materialize at most once, and
publish one new merge state. Schema, opaque, or semantic-key conflicts MUST be
rejected before any worktree, index, or journal mutation. A later call may
replace the table's earlier selection with the other side.

### 8.5 Path resolve-undo

`unresolveMergePath` restores a resolved path's original Base/Ours/Theirs
stages and merge worktree candidate from the active session journal. It clears
that path's row and whole-path selections but retains the journal so the path
can be resolved again. It is valid only during the same active merge and MUST
honor the current state token without side effects on stale input.

### 8.6 Physical projection

Whether a resolution updates an ordinary SQLite worktree path, and how WAL,
locks, sidecars, replacement, and rollback work, is defined exclusively by the
materialization specification. Canonical index/merge-state success MUST not be
claimed if a required projection fails without a valid recovery record.

### 8.7 Application semantic provider

`prepareSemanticMerge` is a non-materializing, token-guarded handoff for an
unresolved SQLite path. The application MUST declare a non-empty bounded list
of provider-managed table names. Graft exports present Base/Ours/Theirs
snapshots as standalone, read-only physical files inside a Graft-owned private
workspace. It also creates a fixed `result.sqlite` candidate from Ours and
applies conflict-free Theirs row changes outside the declared managed tables.
The provider token is bound to the provider name, path, active merge-state
token, frozen policy token/version, three immutable merge revisions, and the
canonical managed-table set. Graft MUST NOT invoke application code or
interpret the business meaning of managed tables.

Seed construction MUST fail before publishing a workspace when the physical
plan contains schema additions or conflicts, opaque or limited changes,
recomputation-required changes, or unresolved row conflicts outside the
managed tables. Failure MUST NOT change the index, worktree, conflict stages,
or merge-state token. `seed_applied_sql` reports whether the safe unmanaged
Theirs projection changed the candidate; `managed_conflicts` reports the
number of row conflicts inside the provider-managed set. These values are
diagnostic and do not replace application validation.

Re-preparing the same unchanged handoff returns the same workspace and provider
record. `recordSemanticMergeConflicts` durably records bounded application
domain conflicts and automatic-resolution audit data without changing the
index, worktree, conflict stages, or merge-state token. Graft treats those
records as opaque JSON and MUST NOT promote them to built-in row/schema rules.

After the application updates and validates the seeded `result.sqlite`,
`acceptSemanticMergeResult` requires both the provider token and current merge
state token. It captures and runs SQLite integrity/foreign-key validation on
the exact result before replacing the application worktree through the normal
materialization boundary and staging one Normal path result. A missing,
non-SQLite, stale, or invalid result MUST leave the original conflict stages
recoverable. The application validation proof and automatic-resolution audit
are bounded opaque records; their business meaning remains owned by the
provider.

This interface is generic. A provider may implement an Eidos metadata policy,
another application's schema policy, or no provider at all. Graft MUST NOT
contain provider-specific table names, clocks, LWW rules, or dependency logic.

## 9. Continue and abort

### 9.1 Continue

`continueMerge` MUST verify:

- an active merge exists and, for token-based adapters, the supplied token is
  current;
- no Base/Ours/Theirs conflict stages remain;
- every required row/schema/opaque resolution is complete;
- the staged SQLite candidates and external payloads are available/valid; and
- expected `HEAD` still equals ours.

It creates one repository commit with two parents, ours first and theirs
second, using the exact staged result and required supplied merge message. Only
after successful commit/ref publication may it clear `MERGE_HEAD`, `ORIG_HEAD`,
and merge-resolution journal. A failure MUST leave a recoverable active merge or a
fully completed commit, never an unlabelled half-state.

### 9.2 Abort

`abortMerge` restores repository state to the recorded original local state,
clears conflict/index merge stages and row-resolution state, and removes merge
records after successful restoration. Physical worktree restoration follows
the materialization specification.

Abort is valid only for an active merge and current adapter-observed state. For
token-based adapters the token must match. If restoration cannot complete,
recovery records MUST remain and the error must identify that manual
retry/recovery is required.

## 10. Fast-forward and up-to-date postconditions

An `up_to_date` apply makes no commit, index, ref, merge-state, or worktree
change and returns the current head.

A `fast_forward` apply moves the current branch/`HEAD` directly to target,
updates index/worktree projection according to checkout rules, and creates no
merge commit or durable active-merge state. If required projection cannot be
performed safely, the ref/worktree operation must obey the atomicity/recovery
contract; it may not report success with an unexplained split state.

Only `three_way` can produce an active merge and later a two-parent commit.

## 11. Concurrency, errors, and recovery

Merge mutation is serialized by repository coordination and expected-state
checks. Concurrent changes to `HEAD`, index, merge token, selected row, or path
state MUST be rejected as stale/repository-busy as appropriate. A client then
reloads status rather than replaying a cached decision blindly.

Cancellation before durable apply has no merge effect. Cancellation after a
durable boundary returns the observable current merge status; it MUST NOT erase
records needed to continue or abort. Remote hydration/publication failures are
reported separately from logical conflicts.

After crash/reopen, the supported recovery choices are always explicit:

```text
inspect status/versions -> continue resolving -> continue
inspect status/versions -> abort
```

No startup routine may auto-select ours/theirs merely to clear a conflict.

## 12. Conformance requirements

A conforming merge implementation MUST test:

1. up-to-date, unborn/normal fast-forward, and divergent three-way topology;
2. plan read-only behavior and malformed/mismatched/stale token rejection;
3. every path-level add/delete/modify relationship, including absent stages;
4. independent and conflicting SQLite row changes;
5. composite keys, safe rowid remap, and semantic-key conflicts;
6. supported/unsupported schema and opaque resolver paths;
7. candidate integrity/foreign-key validation and rollback on failure;
8. whole-path, text, and row resolution with stale-token protection;
9. close/reopen persistence of status, conflict details, selections, and result;
10. semantic-provider prepare/reopen, stale provider/state tokens, bounded
    conflict records, invalid result rejection, validated result acceptance,
    and continue/abort cleanup;
11. continue creating the exact two-parent commit;
12. abort restoring the original repository state; and
13. crash/failure behavior at durable and physical-projection boundaries.

Current evidence lives in `crates/graft/src/repo/merge.rs`, repository merge
tests, SQLite row-merge tests in `crates/graft-sqlite/src/`, command-service
integration tests, Rust SDK tests, Node contract tests, and Playground browser
merge fixtures.

## 13. Known limits

- Unrelated histories use an empty base (`merge_base = null`); there is no
  synthetic or recursive ancestor.
- Semantic-key conflicts require whole-file/manual resolution.
- Per-row selection is not available when schema or opaque conflicts remain.
- Only documented schema/internal resolvers are accepted.
- Multi-path physical projection is recoverable but not a single filesystem
  transaction; see the materialization specification.
