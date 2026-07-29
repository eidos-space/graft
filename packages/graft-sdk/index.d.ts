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
}

export interface DiffPathsOptions extends OperationOptions {
  paths: string[]
  rows?: boolean
  from?: string
  /** Maximum explicit paths returned per page. Range: 1–100. */
  limit?: number
  /** Last path from the preceding page. */
  after?: string
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

export interface RepositoryStatusCounts {
  unstaged: number
  staged: number
  conflicted: number
}

export interface RepositoryStatusPath {
  path: string
  code: string
  conflicted: boolean
  index_status: string
  worktree_status: string
  [key: string]: unknown
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
}

export interface IncrementalStatusResult {
  generation: number
  change_token: string
  status: RepositoryStatus
  telemetry: StatusTelemetry
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

export interface PathDiffResult {
  path: string
  diff: GraftJson
}

export interface DiffTelemetry {
  duration_us: number
  requested_paths: number
  returned_paths: number
  changed_paths: number
}

export interface DiffPathsResult {
  paths: PathDiffResult[]
  has_more: boolean
  next_cursor: string | null
  telemetry: DiffTelemetry
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
  }
}

export interface IgnoredPathResult {
  path: string
  is_ignored: boolean
  is_tracked: boolean
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
  addAll(options?: OperationOptions): Promise<GraftJson>
  stagePaths(options: StagePathsOptions): Promise<BatchPathsResult>
  commit(message: string, options?: OperationOptions): Promise<GraftJson>
  diff(options?: DiffOptions): Promise<GraftJson>
  diffPaths(options: DiffPathsOptions): Promise<DiffPathsResult>
  history(options?: HistoryOptions): Promise<GraftJson>
  historySummaries(options?: HistoryOptions): Promise<HistorySummariesResult>
  commitDetails(
    revision: string,
    options?: OperationOptions
  ): Promise<GraftJson>
  isIgnoredPath(
    path: string,
    options?: OperationOptions
  ): Promise<IgnoredPathResult>
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
