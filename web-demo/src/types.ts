export type OutputStream = "stdout" | "stderr";

export interface CommandOutput {
  line: string;
  stream: OutputStream;
}

export interface CommandResult {
  code: number;
  stderr: string[];
  stdout: string[];
}

export interface RepoStatusPath {
  code: string;
  conflicted: boolean;
  index_status: string;
  kind: "sqlite_database" | "text_file" | "binary_file";
  path: string;
  staged_change?: "added" | "modified" | "deleted";
  storage: "sqlite_snapshot" | "inline" | "external";
  unstaged_change?: "added" | "modified" | "deleted" | "untracked";
  worktree_status: string;
}

export interface RepoStatus {
  counts: {
    conflicted: number;
    staged: number;
    unstaged: number;
  };
  current_branch?: string;
  current_head?: string;
  merge_head?: string;
  orig_head?: string;
  has_conflicts: boolean;
  has_staged_changes: boolean;
  has_unstaged_changes: boolean;
  paths: RepoStatusPath[];
  work_in_progress: boolean;
}

export type ConflictResolution = "manual" | "ours" | "theirs";

export interface RepoConflictPath {
  kind: RepoStatusPath["kind"];
  path: string;
  resolved: number;
  status: "resolved" | "unresolved";
  storage: RepoStatusPath["storage"];
  total: number;
  unresolved: number;
}

export interface RepoConflictArtifact {
  base_row?: unknown[];
  change?: string;
  columns?: string[];
  entry_type?: string;
  id: string;
  kind: "file" | "opaque" | "row" | "schema";
  message?: string;
  name?: string;
  ours_op?: string;
  ours_row?: unknown[];
  path: string;
  path_kind: RepoStatusPath["kind"];
  reason: string;
  resolution?: ConflictResolution;
  rowid?: number;
  semantic_key?: string[];
  status: "resolved" | "unresolved";
  storage: RepoStatusPath["storage"];
  table?: string;
  theirs_op?: string;
  theirs_row?: unknown[];
}

export interface RepoConflictList {
  conflicts: RepoConflictArtifact[];
  current_branch?: string;
  current_head?: string;
  merge_head?: string;
  paths: RepoConflictPath[];
}

export interface RepoTrackedPath {
  kind: RepoStatusPath["kind"];
  page_count?: number;
  path: string;
  size?: number;
  storage: RepoStatusPath["storage"];
}

export interface RepoTrackedPathList {
  paths: RepoTrackedPath[];
}

export type ResetMode = "hard" | "mixed" | "soft";

export interface CommitChange {
  change: "added" | "modified" | "deleted";
  kind: RepoStatusPath["kind"];
  path: string;
  storage: RepoStatusPath["storage"];
}

export interface CommitInfo {
  changes?: CommitChange[];
  id: string;
  message: string;
  parent?: string | null;
  parents?: string[];
  timestamp_ms: number;
}

export interface BranchInfo {
  current: boolean;
  name: string;
  target?: string | null;
}

export interface RepoLog {
  commits: CommitInfo[];
  current_branch?: string;
  current_head?: string;
  has_more: boolean;
}

export interface BranchList {
  branches: BranchInfo[];
  current_branch?: string;
  current_head?: string;
}

export interface OpfsEntry {
  kind: "directory" | "file";
  modified?: number;
  path: string;
  size?: number;
}

interface DiffViewBase {
  description?: string;
  label?: string;
  path: string;
}

export interface TextDiffView extends DiffViewBase {
  after: string;
  before: string;
  kind: "text_file";
}

export interface BinaryDiffView extends DiffViewBase {
  after?: BinaryContentState;
  before?: BinaryContentState;
  change: "added" | "deleted" | "modified" | "untracked";
  kind: "binary_file";
  size?: number;
  storage: RepoStatusPath["storage"];
}

export interface BinaryContentState {
  content?: string;
  content_hash?: string;
  size?: number;
  state: "absent" | "utf8" | "base64" | "too_large" | "missing_payload" | "invalid_utf8";
}

export interface SqliteRowChange {
  old_values?: unknown[];
  op: "insert" | "delete" | "update";
  rowid: number;
  values: unknown[];
}

export interface SqliteTableDiff {
  changes: SqliteRowChange[];
  columns: string[];
  name: string;
}

export interface SqliteDiffView extends DiffViewBase {
  kind: "sqlite_database";
  tables: SqliteTableDiff[];
}

export type DiffView = BinaryDiffView | SqliteDiffView | TextDiffView;
