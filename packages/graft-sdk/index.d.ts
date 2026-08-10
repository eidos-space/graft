export type SessionLifecycle = "closed" | "opening" | "open" | "closing"

export interface OperationOptions {
  signal?: AbortSignal
}

export interface DiffOptions extends OperationOptions {
  rows?: boolean
  staged?: boolean
  root?: string
  from?: string
  to?: string
  path?: string
  /** Return row details only for this table. Requires rows and one explicit path. */
  table?: string
}

export interface DiffPathsOptions extends OperationOptions {
  paths: string[]
  rows?: boolean
  /** Compare an empty tree to this root commit. Mutually exclusive with from/to. */
  root?: string
  /** Compare this revision to `to`, or to the worktree when `to` is omitted. */
  from?: string
  /** Historical comparison target. Requires `from`. */
  to?: string
  /** Return row details only for this table. Requires rows. */
  table?: string
  /** Maximum explicit paths returned per page. Range: 1–100. */
  limit?: number
  /** Last path from the preceding page. */
  after?: string
}

interface SqliteDiffPathsBase extends OperationOptions {
  paths: string[]
  /** Compare HEAD to the staged index. Mutually exclusive with root/from/to. */
  staged?: boolean
  /** Use the staged diff when the worktree has no change for the requested path. */
  stagedFallback?: boolean
  root?: string
  from?: string
  to?: string
  /** Maximum explicit paths returned per page. Range: 1–100. */
  limit?: number
  /** Last path from the preceding path page. */
  after?: string
}

export type SqliteDiffPathsOptions =
  | (SqliteDiffPathsBase & {
      mode: "summary"
      table?: never
      rowLimit?: never
      rowAfter?: never
    })
  | (SqliteDiffPathsBase & {
      mode: "rows"
      table: string
      /** Maximum row changes returned. Range: 1–1,000. Defaults to 100. */
      rowLimit?: number
      /** Opaque cursor returned by the preceding row page. */
      rowAfter?: string
    })

export interface ReadPathContentOptions extends OperationOptions {
  /** Commit id, ref, or other supported immutable revision expression. */
  revision: string
  /** One normalized repository-relative artifact path. */
  path: string
  /** UTF-8 content limit in bytes. Range: 1–8,388,608. */
  maxBytes: number
}

export interface HistoryOptions extends OperationOptions {
  limit?: number
  after?: string
}

export interface RestoreOptions extends OperationOptions {
  source?: string
  expectedHead?: string
  requireClean?: boolean
  path: string
}

export interface StagePathsOptions extends OperationOptions {
  paths: string[]
  expectedHead?: string
  force?: boolean
}

export interface RecordPathMoveOptions extends OperationOptions {
  /** Tracked repository-relative path before the completed physical rename. */
  previousPath: string
  /** Repository-relative path after the completed physical rename. */
  path: string
  /** Compare-and-swap guard; the operation fails if HEAD differs. */
  expectedHead?: string
}

export interface RecordPathMoveResult {
  previous_path: string
  path: string
  change: "renamed"
  materializes_worktree: false
}

export interface UntrackPathsOptions extends OperationOptions {
  /** One to 1,000 normalized explicit file paths. Directories are rejected. */
  paths: string[]
  /** Compare-and-swap guard; the operation fails if HEAD differs. */
  expectedHead?: string
}

export interface RestorePathsOptions extends OperationOptions {
  source?: string
  expectedHead?: string
  requireClean?: boolean
  paths: string[]
}

export interface RemoteConfigureOptions extends OperationOptions {
  name: string
  url: string
  bearerToken?: string
  overwrite?: boolean
  upstreamBranch?: string
}

export interface RemoteOperationOptions extends OperationOptions {
  remote?: string
  branch?: string
}

export interface CloneOptions extends OperationOptions {
  remoteUrl: string
  branch?: string
  bearerToken?: string
}

export type GraftJson = Record<string, unknown> | unknown[]

export type RepositoryPathKind =
  | "sqlite_database"
  | "text_file"
  | "binary_file"

export type RepositoryPathStorage =
  | "sqlite_snapshot"
  | "inline"
  | "external"

export interface RepositoryStatusCounts {
  unstaged: number
  staged: number
  conflicted: number
}

export interface RepositoryStatusPath {
  path: string
  previous_path?: string
  kind: RepositoryPathKind
  storage: RepositoryPathStorage
  code: string
  conflicted: boolean
  index_status: string
  worktree_status: string
  unstaged_change?: "modified" | "deleted" | "untracked"
  staged_change?: "added" | "modified" | "deleted" | "renamed"
  [key: string]: unknown
}

export type RepositoryUpstreamState =
  | "up_to_date"
  | "ahead"
  | "behind"
  | "diverged"

export interface RepositoryUpstreamStatus {
  remote: string
  branch: string
  local: string
  remote_target: string
  /** Present when state is `diverged`. */
  common_ancestor?: string
  ahead: number
  behind: number
  state: RepositoryUpstreamState
}

export interface RepositoryStatus {
  worktree: string
  graft_dir: string
  repository_format_version: number
  head_target: string | null
  merge_head: string | null
  orig_head: string | null
  dirty: boolean
  has_unstaged_changes: boolean
  has_staged_changes: boolean
  has_conflicts: boolean
  work_in_progress: boolean
  counts: RepositoryStatusCounts
  paths: RepositoryStatusPath[]
  unstaged: string[]
  staged: string[]
  conflicted: string[]
  upstream: { remote: string; branch: string } | null
  upstream_status?: RepositoryUpstreamStatus
  ahead: number
  behind: number
  [key: string]: unknown
}

export interface StatusResult extends RepositoryStatus {
  current_head?: string
  current_branch?: string
}

export interface StatusTelemetry {
  duration_us: number
  paths_examined: number
  metadata_cache_hits: number
  metadata_cache_misses: number
  tree_cache_hit: boolean
  status_cache_hit: boolean
  persistent_snapshot_hit: boolean
  persistent_snapshot_saved: boolean
  stability_retries: number
}

export interface IncrementalStatusResult {
  generation: number
  change_token: string
  status: RepositoryStatus
  telemetry: StatusTelemetry
}

export interface RepositoryMetadataTelemetry {
  duration_us: number
  /** Metadata-only operations never examine worktree paths. */
  paths_examined: 0
}

export interface RepositoryMetadataResult {
  current_head: string | null
  current_branch: string | null
  upstream: { remote: string; branch: string } | null
  repository_format_version: number
  object_format: string
  telemetry: RepositoryMetadataTelemetry
}

export interface SafeRemoteInfo {
  name: string
  kind: "memory" | "fs" | "s3_compatible" | "http"
  /** Credential-free configured remote URL. HTTP token_env is intentionally omitted. */
  url: string
}

export interface ListRemotesResult {
  remotes: SafeRemoteInfo[]
  telemetry: RepositoryMetadataTelemetry
}

export interface CommitPathChangeCounts {
  added: number
  modified: number
  deleted: number
}

export interface CommitTableSummary {
  name: string
  inserts: number
  deletes: number
  updates: number
}

export interface CommitSummary {
  id: string
  parents: string[]
  message: string
  timestamp_ms: number
  path_changes: CommitPathChangeCounts | null
  path_counts_complete: boolean
  tables: CommitTableSummary[]
  changed_tables: number
}

export interface HistoryTelemetry {
  duration_us: number
  commits_returned: number
  tree_objects_read: 0
  blob_objects_read: 0
}

export interface HistorySummariesResult {
  commits: CommitSummary[]
  has_more: boolean
  next_cursor: string | null
  telemetry: HistoryTelemetry
}

export interface CommitChangedPathsOptions extends OperationOptions {
  revision: string
  /** Maximum changed paths returned per page. Range: 1–100. */
  limit?: number
  /** Last path from the preceding page. */
  after?: string
}

export interface CommitPathChange {
  path: string
  previous_path?: string
  change: "added" | "modified" | "deleted" | "renamed"
  kind: RepositoryPathKind
  storage: RepositoryPathStorage
}

export interface CommitChangedPathsResult {
  /** Resolved commit id. */
  revision: string
  /** First parent id, or null when this is a root commit. */
  parent: string | null
  paths: CommitPathChange[]
  total_changed_paths: number
  has_more: boolean
  next_cursor: string | null
  telemetry: {
    duration_us: number
    paths_examined: number
    items_returned: number
    tree_objects_read: number
    blob_objects_read: 0
  }
}

export interface PathDiffResult {
  path: string
  diff: GraftJson
}

export interface DiffTelemetry {
  duration_us: number
  requested_paths: number
  returned_paths: number
  changed_paths: number
  /** True when explicit paths use path-local tree/index state instead of full map hydration. */
  path_filter_fast_path: boolean
  /** True when row details were limited to the requested table. */
  table_filter_fast_path: boolean
  /** Requested table for row details, or null for an all-table diff. */
  requested_table: string | null
  /** Number of SQLite tables whose rows were scanned across the returned paths. */
  tables_scanned: number
  /** Always zero for the bounded path-local implementation. */
  full_tree_paths_hydrated: number
}

export interface DiffPathsResult {
  paths: PathDiffResult[]
  has_more: boolean
  next_cursor: string | null
  telemetry: DiffTelemetry
}

export interface SqliteTableSummary {
  name: string
  inserts: number
  deletes: number
  updates: number
}

export interface SqliteRowChange {
  op: "insert" | "delete" | "update"
  rowid?: number
  key?: Record<string, unknown>
  values: unknown[]
  old_values?: unknown[]
}

export interface SqliteRowChangeTable {
  name: string
  columns?: string[]
  primary_key_columns?: string[]
  changes: SqliteRowChange[]
}

export interface BoundedSqliteDiffFile {
  path: string
  previous_path?: string
  change: "added" | "modified" | "deleted" | "renamed"
  kind: RepositoryPathKind
  storage: RepositoryPathStorage
  row_diff_available: boolean
  mode: "summary" | "rows"
  logical_status: string
  capabilities: string[]
  limitations: Array<{ kind: string; subject?: string }>
  message?: string
  summaries?: SqliteTableSummary[]
  tables?: SqliteRowChangeTable[]
  opaque_changes?: unknown[]
  has_more: boolean
  next_cursor?: string
  telemetry: {
    requested_table?: string
    tables_considered: number
    tables_scanned: number
    rows_scanned: number
    rows_returned: number
    truncated: boolean
    response_scope:
      | "streaming_rowid"
      | "streaming_primary_key"
      | "streaming_btree"
      | "materialized_compat"
      | "unavailable"
  }
}

export interface BoundedSqliteRepositoryDiff {
  current_head?: string
  current_branch?: string
  from: string
  to: string
  paths: Array<{
    path: string
    previous_path?: string
    change: "added" | "modified" | "deleted" | "renamed"
    kind: RepositoryPathKind
    storage: RepositoryPathStorage
  }>
  files: BoundedSqliteDiffFile[]
}

export interface SqliteDiffPathsResult {
  paths: Array<{ path: string; diff: BoundedSqliteRepositoryDiff }>
  has_more: boolean
  next_cursor: string | null
  telemetry: {
    duration_us: number
    requested_paths: number
    returned_paths: number
    changed_paths: number
    response_scope: string
    requested_table: string | null
    tables_scanned: number
    rows_scanned: number
    rows_returned: number
    truncated: boolean
  }
}

export type PathContentState =
  | { state: "absent" }
  | {
      state: "utf8"
      content: string
      size: number
      content_hash: string
    }
  | {
      state: "too_large" | "missing_payload" | "invalid_utf8"
      size: number
      content_hash: string
    }

export interface ReadPathContentResult {
  /** Fully resolved commit id. */
  revision: string
  path: string
  kind: RepositoryPathKind | null
  storage: RepositoryPathStorage | null
  content: PathContentState
}

export interface BatchPathResult {
  path: string
  result: GraftJson
}

export interface BatchPathsResult {
  paths: BatchPathResult[]
  materializes_worktree: boolean
}

export type InventoryKind =
  | "tracked"
  | "untracked"
  | "ignored"
  | "tracked_ignored"

export interface InventoryOptions extends OperationOptions {
  kind?: InventoryKind
  /** Maximum paths returned per page. Range: 1–1000. */
  limit?: number
  after?: string
}

export interface InventoryItem {
  path: string
  tracked: boolean
  ignored: boolean
}

export interface IgnoreMigrationDiagnostic {
  ignored_rules_do_not_untrack: true
  tracked_ignored_paths: number
  recommendation: string
}

export interface InventoryResult {
  kind: InventoryKind
  items: InventoryItem[]
  total_matching: number
  has_more: boolean
  next_cursor: string | null
  migration: IgnoreMigrationDiagnostic | null
  telemetry: {
    duration_us: number
    paths_examined: number
    items_returned: number
    inventory_cache_hit: boolean
    index_cache_hit: boolean
    ignore_matcher_cache_hit: boolean
  }
}

export interface IgnoredPathResult {
  path: string
  is_ignored: boolean
  is_tracked: boolean
  /** True for a physical directory or a path with tracked descendants. */
  is_directory: boolean
  /** True when the index contains one or more files below this directory path. */
  has_tracked_descendants: boolean
}

export interface IgnoredPathsOptions extends OperationOptions {
  /** One to 1,000 normalized repository-relative file or directory paths. */
  paths: string[]
}

export interface IgnoredPathsResult {
  paths: IgnoredPathResult[]
  telemetry: {
    duration_us: number
    paths_examined: number
    index_cache_hit: boolean
    ignore_matcher_cache_hit: boolean
  }
}

export class GraftSdkError extends Error {
  readonly code: string
  readonly cause?: unknown
}

export class RepositorySession {
  constructor(target: string)
  static open(
    target: string,
    options?: OperationOptions
  ): Promise<RepositorySession>

  readonly target: string
  readonly lifecycle: SessionLifecycle

  open(options?: OperationOptions): Promise<SessionLifecycle>
  close(options?: OperationOptions): Promise<SessionLifecycle>
  reopen(options?: OperationOptions): Promise<SessionLifecycle>

  setHttpBearerToken(remoteName: string, token: string): void
  clearHttpBearerToken(remoteName: string): void

  init(options?: OperationOptions): Promise<GraftJson>
  status(options?: OperationOptions): Promise<StatusResult>
  statusIncremental(
    options?: OperationOptions
  ): Promise<IncrementalStatusResult>
  repositoryMetadata(
    options?: OperationOptions
  ): Promise<RepositoryMetadataResult>
  listRemotes(options?: OperationOptions): Promise<ListRemotesResult>
  addAll(options?: OperationOptions): Promise<GraftJson>
  stagePaths(options: StagePathsOptions): Promise<BatchPathsResult>
  recordPathMove(options: RecordPathMoveOptions): Promise<RecordPathMoveResult>
  untrackPaths(options: UntrackPathsOptions): Promise<BatchPathsResult>
  commit(message: string, options?: OperationOptions): Promise<GraftJson>
  diff(options?: DiffOptions): Promise<GraftJson>
  diffPaths(options: DiffPathsOptions): Promise<DiffPathsResult>
  diffSqlitePaths(options: SqliteDiffPathsOptions): Promise<SqliteDiffPathsResult>
  readPathContent(options: ReadPathContentOptions): Promise<ReadPathContentResult>
  history(options?: HistoryOptions): Promise<GraftJson>
  historySummaries(options?: HistoryOptions): Promise<HistorySummariesResult>
  commitDetails(
    revision: string,
    options?: OperationOptions
  ): Promise<GraftJson>
  commitChangedPaths(
    options: CommitChangedPathsOptions
  ): Promise<CommitChangedPathsResult>
  isIgnoredPath(
    path: string,
    options?: OperationOptions
  ): Promise<IgnoredPathResult>
  isIgnoredPaths(options: IgnoredPathsOptions): Promise<IgnoredPathsResult>
  inventory(options?: InventoryOptions): Promise<InventoryResult>
  restore(options: RestoreOptions): Promise<GraftJson>
  restorePaths(options: RestorePathsOptions): Promise<BatchPathsResult>
  configureRemote(options: RemoteConfigureOptions): Promise<GraftJson>
  push(options?: RemoteOperationOptions): Promise<GraftJson>
  fetch(options?: RemoteOperationOptions): Promise<GraftJson>
  pull(options?: RemoteOperationOptions): Promise<GraftJson>
  cloneRepository(options: CloneOptions): Promise<GraftJson>
}

export function operationMaterializesWorktree(operation: string): boolean
export function sdkVersion(): string
