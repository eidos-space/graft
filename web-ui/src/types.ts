export type Mode = "files" | "changes" | "history";
export interface TreeSelection {
  path: string;
  table?: string;
}
export interface DatabaseTable {
  name: string;
  inserts?: number;
  updates?: number;
  deletes?: number;
  schema_changed?: boolean;
  change?: string;
  opaque?: boolean;
}
export interface DatabaseCatalog {
  tables: DatabaseTable[];
  default_table?: string;
  error?: string;
}
export interface ChangedPath {
  path: string;
  previous_path?: string;
  kind?: string;
  change?: string;
  staged_change?: string;
  unstaged_change?: string;
  worktree_status?: string;
  conflicted?: boolean;
}
export interface Snapshot {
  root: string;
  paths: string[];
  databases: string[];
  truncated: boolean;
  merge?: { state: "none" | "merging"; unmerged_count?: number };
  status: {
    current_head?: string;
    current_branch?: string;
    paths: ChangedPath[];
  };
}
export interface Commit {
  id: string;
  message: string;
  timestamp_ms: number;
  parents: string[];
}
export interface HistoryPage {
  commits: Commit[];
  has_more: boolean;
  next_cursor?: string;
}
export interface CommitPage {
  paths: ChangedPath[];
  parent?: string;
  has_more: boolean;
  next_cursor?: string;
}
export interface Content {
  state: "utf8" | "absent" | "binary" | "sqlite" | "invalid_utf8" | "too_large" | "missing_payload";
  content?: string;
  etag?: string;
  content_hash?: string;
  size?: number;
}
export interface FilePair {
  before: Content;
  after: Content;
}
export interface RowChange {
  op: "insert" | "update" | "delete";
  rowid?: number;
  key?: Record<string, unknown>;
  values: unknown[];
  old_values?: unknown[];
}
export interface TableSummary {
  name: string;
  inserts: number;
  updates: number;
  deletes: number;
  schema_changed?: boolean;
}
export interface DatabaseDiff {
  summaries: TableSummary[];
  tables?: {
    name: string;
    columns: string[];
    rowid_alias?: string;
    primary_key_columns?: string[];
    changes: RowChange[];
  }[];
  next_cursor?: string;
  has_more?: boolean;
  truncated?: boolean;
  metadata?: unknown;
  schema_changes?: {
    name: string;
    entry_type: string;
    op: string;
    sql: string;
    old_sql?: string;
  }[];
  opaque_changes?: { name: string; change: string; reason: string }[];
  limitations?: { kind: string; subject?: string }[];
  message?: string;
}
export interface DatabasePage {
  version?: unknown;
  truncated_cells?: number[][];
  total_rows: number;
  tables: string[];
  table?: string;
  columns: string[];
  rows: unknown[][];
  has_more: boolean;
  offset: number;
}
export type ApiInput = Record<string, unknown>;
