//! Long-lived, serialized repository sessions for embedding Graft.
//!
//! This crate is the stable boundary between Graft's repository command implementation and
//! language bindings. It deliberately reuses [`graft_sqlite::repo_service`] rather than
//! reimplementing repository or remote protocols.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU8, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub use graft::repo::{
    CancellationToken, RepoPathContent, RepoPathContentState, TransferDirection, TransferProgress,
    TransferProgressReporter,
};
use graft::repo::{
    CommitArtifactState, CommitFileState, MergeOutcome, MergePlan, RepoPathStorage, RepoStatus,
    RepoTrackedPathKind, Repository,
    index::{Index, IndexEntry, IndexStage},
};
pub use graft::repo::{ManagedColumnResolver, MergeConfig, SemanticKeyCollation};
use graft::{
    core::byte_unit::ByteUnit,
    remote::{RemoteConfig, RemoteCredentialErr, RemoteCredentials},
};
use graft_sqlite::{
    error::ErrCtx,
    repo_service::{
        RepositoryCommand, RepositoryCommandService, RepositoryMergePolicy as ServiceMergePolicy,
        RepositoryResolveCellOptions as ServiceResolveCellOptions,
        RepositoryResolveOptions as ServiceResolveOptions,
        RepositoryResolveRow as ServiceResolveRow, RepositoryResolveSide as ServiceResolveSide,
        RepositoryResolveTableOptions as ServiceResolveTableOptions,
    },
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

const LIFECYCLE_CLOSED: u8 = 0;
const LIFECYCLE_OPENING: u8 = 1;
const LIFECYCLE_OPEN: u8 = 2;
const LIFECYCLE_CLOSING: u8 = 3;
const MAX_HISTORY_SUMMARY_PAGE_SIZE: usize = 500;
const MAX_COMMIT_CHANGED_PATH_PAGE_SIZE: usize = 100;
const MAX_DIFF_PATH_PAGE_SIZE: usize = 100;
const MAX_DIFF_PATH_REQUEST_SIZE: usize = 10_000;
pub const MAX_PATH_CONTENT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_BATCH_MUTATION_PATHS: usize = 1_000;
const MAX_INVENTORY_PAGE_SIZE: usize = 1_000;
const MAX_IGNORE_QUERY_PATHS: usize = 1_000;
const MAX_MERGE_PATH_PAGE_SIZE: usize = 500;
const MAX_MERGE_CONFLICT_PAGE_SIZE: usize = 1_000;
const SEMANTIC_MERGE_WORKSPACE_VERSION: u32 = 1;
const MAX_SEMANTIC_MANAGED_TABLES: usize = 256;
const SEMANTIC_MERGE_WORKSPACE_DIRECTORY: &str = "semantic-merge";
const MAX_SEMANTIC_PROVIDER_NAME_BYTES: usize = 128;
const MAX_SEMANTIC_MERGE_RECORD_BYTES: usize = 1024 * 1024;
// Bump whenever persisted path classification semantics change.
const STATUS_SNAPSHOT_SCHEMA_VERSION: u32 = 3;
const MAX_STATUS_SNAPSHOTS: usize = 4;
const STATUS_SNAPSHOT_MAX_BYTES: u64 = 256 * 1024 * 1024;
const WORKTREE_STABILITY_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycle {
    Closed,
    Opening,
    Open,
    Closing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SdkErrorCode {
    SessionClosed,
    SessionOpening,
    SessionClosing,
    SessionAlreadyOpen,
    RepositoryBusy,
    Cancelled,
    InvalidArgument,
    InvalidResponse,
    RepositoryStale,
    RemoteTransportTimeout,
    RemotePublicationUnconfirmed,
    RemotePublicationOutcomeUnknown,
    RepositoryCommand,
}

impl SdkErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionClosed => "GRAFT_SDK_SESSION_CLOSED",
            Self::SessionOpening => "GRAFT_SDK_SESSION_OPENING",
            Self::SessionClosing => "GRAFT_SDK_SESSION_CLOSING",
            Self::SessionAlreadyOpen => "GRAFT_SDK_SESSION_ALREADY_OPEN",
            Self::RepositoryBusy => "GRAFT_SDK_REPOSITORY_BUSY",
            Self::Cancelled => "GRAFT_SDK_CANCELLED",
            Self::InvalidArgument => "GRAFT_SDK_INVALID_ARGUMENT",
            Self::InvalidResponse => "GRAFT_SDK_INVALID_RESPONSE",
            Self::RepositoryStale => "GRAFT_SDK_REPOSITORY_STALE",
            Self::RemoteTransportTimeout => "GRAFT_SDK_REMOTE_TRANSPORT_TIMEOUT",
            Self::RemotePublicationUnconfirmed => "GRAFT_SDK_REMOTE_PUBLICATION_UNCONFIRMED",
            Self::RemotePublicationOutcomeUnknown => "GRAFT_SDK_REMOTE_PUBLICATION_OUTCOME_UNKNOWN",
            Self::RepositoryCommand => "GRAFT_SDK_REPOSITORY_COMMAND",
        }
    }
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct SdkError {
    code: SdkErrorCode,
    message: String,
}

impl SdkError {
    pub fn code(&self) -> SdkErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    fn new(code: SdkErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

pub type Result<T> = std::result::Result<T, SdkError>;

/// Installs a cancellation token for one synchronous SDK operation on the current worker thread.
pub fn with_cancellation<T>(token: &CancellationToken, operation: impl FnOnce() -> T) -> T {
    graft::repo::with_cancellation(token, operation)
}

/// Installs transfer progress reporting for one synchronous SDK operation on the worker thread.
pub fn with_transfer_progress<T>(
    reporter: &TransferProgressReporter,
    operation: impl FnOnce() -> T,
) -> T {
    graft::repo::with_transfer_progress(reporter, operation)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusTelemetry {
    pub duration_us: u64,
    pub paths_examined: usize,
    pub metadata_cache_hits: usize,
    pub metadata_cache_misses: usize,
    pub tree_cache_hit: bool,
    pub status_cache_hit: bool,
    pub persistent_snapshot_hit: bool,
    pub persistent_snapshot_saved: bool,
    pub stability_retries: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncrementalStatusResult {
    pub generation: u64,
    pub change_token: String,
    pub status: RepoStatus,
    pub telemetry: StatusTelemetry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryTelemetry {
    pub duration_us: u64,
    pub commits_returned: usize,
    pub tree_objects_read: usize,
    pub blob_objects_read: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistorySummariesResult {
    pub commits: Vec<graft::repo::RepoCommitSummary>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub telemetry: HistoryTelemetry,
}

#[derive(Debug, Clone)]
pub struct CommitChangedPathsOptions {
    pub revision: String,
    pub limit: usize,
    pub after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitChangedPathsTelemetry {
    pub duration_us: u64,
    pub paths_examined: usize,
    pub items_returned: usize,
    pub tree_objects_read: usize,
    pub blob_objects_read: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitChangedPathsResult {
    pub revision: String,
    pub parent: Option<String>,
    pub paths: Vec<graft::repo::CommitPathChange>,
    pub total_changed_paths: usize,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub telemetry: CommitChangedPathsTelemetry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryKind {
    Tracked,
    Untracked,
    Ignored,
    TrackedIgnored,
}

#[derive(Debug, Clone)]
pub struct InventoryOptions {
    pub kind: InventoryKind,
    pub limit: usize,
    pub after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryItem {
    pub path: String,
    pub tracked: bool,
    pub ignored: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IgnoreMigrationDiagnostic {
    pub ignored_rules_do_not_untrack: bool,
    pub tracked_ignored_paths: usize,
    pub recommendation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryTelemetry {
    pub duration_us: u64,
    pub paths_examined: usize,
    pub items_returned: usize,
    pub inventory_cache_hit: bool,
    pub index_cache_hit: bool,
    pub ignore_matcher_cache_hit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryResult {
    pub kind: InventoryKind,
    pub items: Vec<InventoryItem>,
    pub total_matching: usize,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub migration: Option<IgnoreMigrationDiagnostic>,
    pub telemetry: InventoryTelemetry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IgnoredPathResult {
    pub path: String,
    pub is_ignored: bool,
    pub is_tracked: bool,
    pub is_directory: bool,
    pub has_tracked_descendants: bool,
}

#[derive(Debug, Clone)]
pub struct IgnoredPathsOptions {
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IgnoredPathsTelemetry {
    pub duration_us: u64,
    pub paths_examined: usize,
    pub index_cache_hit: bool,
    pub ignore_matcher_cache_hit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IgnoredPathsResult {
    pub paths: Vec<IgnoredPathResult>,
    pub telemetry: IgnoredPathsTelemetry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryMetadataTelemetry {
    pub duration_us: u64,
    pub paths_examined: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryMetadataResult {
    pub current_head: Option<String>,
    pub current_branch: Option<String>,
    pub upstream: Option<graft::repo::BranchUpstream>,
    pub upstream_target: Option<String>,
    pub repository_format_version: u32,
    pub object_format: String,
    pub telemetry: RepositoryMetadataTelemetry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeRemoteKind {
    Memory,
    Fs,
    S3Compatible,
    Http,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafeRemoteInfo {
    pub name: String,
    pub kind: SafeRemoteKind,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListRemotesResult {
    pub remotes: Vec<SafeRemoteInfo>,
    pub telemetry: RepositoryMetadataTelemetry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryOperation {
    Init,
    Status,
    StatusIncremental,
    AddAll,
    StagePaths,
    RecordPathMove,
    UntrackPaths,
    Commit,
    Diff,
    DiffPaths,
    ReadPathContent,
    History,
    HistorySummaries,
    CommitDetails,
    CommitChangedPaths,
    IsIgnoredPath,
    IsIgnoredPaths,
    Inventory,
    RepositoryMetadata,
    ListRemotes,
    Restore,
    RestorePaths,
    RemoteConfigure,
    Push,
    Fetch,
    Pull,
    Clone,
    PlanMerge,
    GetMergePolicy,
    ValidateMergePolicy,
    SetMergePolicy,
    ApplyMerge,
    GetMergeStatus,
    ListMergePaths,
    ListMergeConflicts,
    ReadMergeVersion,
    DiffMergeSqlite,
    SetMergePathResult,
    UnresolveMergePath,
    ResolveMergeRow,
    ResolveMergeCell,
    ResolveMergeTable,
    StageMergeSqliteResult,
    PrepareSemanticMerge,
    RecordSemanticMergeConflicts,
    AcceptSemanticMergeResult,
    WriteAndStageTextResult,
    ContinueMerge,
    AbortMerge,
}

impl RepositoryOperation {
    /// Whether the operation can replace, create, or remove physical worktree files.
    pub const fn materializes_worktree(self) -> bool {
        matches!(
            self,
            Self::Restore
                | Self::RestorePaths
                | Self::Pull
                | Self::Clone
                | Self::ApplyMerge
                | Self::SetMergePathResult
                | Self::UnresolveMergePath
                | Self::ResolveMergeRow
                | Self::ResolveMergeCell
                | Self::ResolveMergeTable
                | Self::AcceptSemanticMergeResult
                | Self::WriteAndStageTextResult
                | Self::ContinueMerge
                | Self::AbortMerge
        )
    }
}

#[derive(Debug, Clone)]
pub struct PlanMergeOptions {
    pub revision: String,
    pub expected_head: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ApplyMergeOptions {
    pub revision: String,
    /// The HEAD observed while planning, or `None` for an unborn branch.
    pub expected_head: Option<String>,
    pub plan_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePolicyDocument {
    pub version: u32,
    #[serde(flatten)]
    pub config: MergeConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePolicyResult {
    pub policy: MergePolicyDocument,
    pub policy_token: String,
    pub active_merge: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePolicyValidationIssue {
    pub key: String,
    pub value: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePolicyValidationResult {
    pub valid: bool,
    pub policy: Option<MergePolicyDocument>,
    pub policy_token: Option<String>,
    pub errors: Vec<MergePolicyValidationIssue>,
}

#[derive(Debug, Clone)]
pub struct SetMergePolicyOptions {
    pub policy: MergePolicyDocument,
    pub expected_policy_token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergePlanKind {
    UpToDate,
    FastForward,
    ThreeWay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePlanResult {
    pub kind: MergePlanKind,
    pub expected_head: Option<String>,
    pub target: String,
    pub merge_base: Option<String>,
    pub staged_paths: Vec<String>,
    pub conflicted_paths: Vec<String>,
    pub plan_token: String,
    pub policy_token: String,
    pub policy_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MergeStatus {
    None,
    Merging {
        orig_head: String,
        merge_head: String,
        merge_base: Option<String>,
        staged_count: usize,
        unmerged_count: usize,
        state_token: String,
        policy_token: String,
        policy_version: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeApplyResult {
    pub plan: MergePlanResult,
    pub output: Value,
    pub merge: MergeStatus,
    #[serde(default)]
    pub worktree_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergePathFilter {
    All,
    Unmerged,
    Resolved,
}

#[derive(Debug, Clone)]
pub struct ListMergePathsOptions {
    pub filter: MergePathFilter,
    pub limit: usize,
    pub after: Option<String>,
    pub expected_state_token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergePathState {
    Unmerged,
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePath {
    pub path: String,
    pub state: MergePathState,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
    pub has_base: bool,
    pub has_ours: bool,
    pub has_theirs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePathPage {
    pub state_token: String,
    pub items: Vec<MergePath>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ListMergeConflictsOptions {
    pub path: PathBuf,
    pub limit: usize,
    pub after: Option<String>,
    pub expected_state_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeConflictPage {
    pub state_token: String,
    pub path: String,
    pub items: Vec<Value>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeVersion {
    Base,
    Ours,
    Theirs,
    Result,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeSqliteVersion {
    Base,
    Ours,
    Theirs,
}

#[derive(Debug, Clone)]
pub struct ReadMergeVersionOptions {
    pub path: PathBuf,
    pub version: MergeVersion,
    pub max_bytes: u64,
    pub expected_state_token: String,
}

#[derive(Debug, Clone)]
pub struct DiffMergeSqliteOptions {
    pub path: PathBuf,
    pub from: MergeSqliteVersion,
    pub to: MergeSqliteVersion,
    pub response: SqliteDiffResponse,
    pub expected_state_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeSqliteDiffEndpoint {
    pub version: MergeSqliteVersion,
    pub revision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeSqliteDiffResult {
    pub state_token: String,
    pub path: String,
    pub from: MergeSqliteDiffEndpoint,
    pub to: MergeSqliteDiffEndpoint,
    pub diff: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MergeContentState {
    Absent,
    Utf8 { content: String, size: u64 },
    TooLarge { size: u64 },
    MissingPayload { size: u64 },
    InvalidUtf8 { size: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeContent {
    pub version: String,
    pub revision: Option<String>,
    pub path: String,
    pub kind: Option<RepoTrackedPathKind>,
    pub storage: Option<RepoPathStorage>,
    pub content: MergeContentState,
    pub state_token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergePathResult {
    Ours,
    Theirs,
}

#[derive(Debug, Clone)]
pub struct SetMergePathResultOptions {
    pub path: PathBuf,
    pub result: MergePathResult,
    pub expected_state_token: String,
}

#[derive(Debug, Clone)]
pub struct ResolveMergeRowOptions {
    pub path: PathBuf,
    pub table: String,
    pub identity: Value,
    pub result: MergePathResult,
    pub expected_state_token: String,
}

#[derive(Debug, Clone)]
pub struct ResolveMergeCellOptions {
    pub path: PathBuf,
    pub table: String,
    pub identity: Value,
    pub column: String,
    pub result: MergePathResult,
    pub expected_state_token: String,
}

#[derive(Debug, Clone)]
pub struct ResolveMergeTableOptions {
    pub path: PathBuf,
    pub table: String,
    pub result: MergePathResult,
    pub expected_state_token: String,
}

#[derive(Debug, Clone)]
pub struct UnresolveMergePathOptions {
    pub path: PathBuf,
    pub expected_state_token: String,
}

#[derive(Debug, Clone)]
pub struct StageMergeSqliteResultOptions {
    pub path: PathBuf,
    pub expected_state_token: String,
}

#[derive(Debug, Clone)]
pub struct PrepareSemanticMergeOptions {
    pub path: PathBuf,
    pub provider: String,
    pub managed_tables: Vec<String>,
    pub expected_state_token: String,
}

#[derive(Debug, Clone)]
pub struct RecordSemanticMergeConflictsOptions {
    pub provider_token: String,
    pub conflicts: Vec<Value>,
    pub automatic_resolutions: Vec<Value>,
    pub expected_state_token: String,
}

#[derive(Debug, Clone)]
pub struct AcceptSemanticMergeResultOptions {
    pub provider_token: String,
    pub validation: Value,
    pub automatic_resolutions: Vec<Value>,
    pub expected_state_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticMergeInput {
    pub version: MergeSqliteVersion,
    pub revision: Option<String>,
    pub file_path: Option<String>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SemanticMergeProviderRecord {
    Pending,
    Conflict {
        conflicts: Vec<Value>,
        automatic_resolutions: Vec<Value>,
    },
    Merged {
        validation: Value,
        automatic_resolutions: Vec<Value>,
    },
}

impl Eq for SemanticMergeProviderRecord {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticMergeWorkspace {
    pub provider_token: String,
    pub provider: String,
    pub path: String,
    pub workspace_path: String,
    pub result_path: String,
    pub managed_tables: Vec<String>,
    pub seed_applied_sql: bool,
    pub managed_conflicts: usize,
    /// Host clock sampled once when this durable provider plan is created.
    pub prepared_at_unix_ms: u64,
    pub state_token: String,
    pub policy_token: String,
    pub policy_version: u32,
    pub orig_head: String,
    pub merge_head: String,
    pub merge_base: Option<String>,
    pub inputs: Vec<SemanticMergeInput>,
    pub record: SemanticMergeProviderRecord,
}

impl Eq for SemanticMergeWorkspace {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SemanticMergeWorkspaceManifest {
    version: u32,
    workspace: SemanticMergeWorkspace,
}

#[derive(Debug, Clone)]
pub struct WriteAndStageTextResultOptions {
    pub path: PathBuf,
    pub content: String,
    pub expected_state_token: String,
}

#[derive(Debug, Clone)]
pub struct ContinueMergeOptions {
    pub message: String,
    pub expected_state_token: String,
}

#[derive(Debug, Clone)]
pub struct AbortMergeOptions {
    pub expected_state_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeOperationResult {
    pub output: Value,
    pub merge: MergeStatus,
    #[serde(default)]
    pub worktree_paths: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DiffOptions {
    pub rows: bool,
    pub staged: bool,
    pub root: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub path: Option<PathBuf>,
    pub table: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DiffPathsOptions {
    pub paths: Vec<PathBuf>,
    pub rows: bool,
    pub root: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub table: Option<String>,
    pub limit: usize,
    pub after: Option<String>,
}

#[derive(Debug, Clone)]
pub enum SqliteDiffResponse {
    Summary,
    Rows {
        table: String,
        limit: usize,
        after: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct SqliteDiffPathsOptions {
    pub paths: Vec<PathBuf>,
    pub staged: bool,
    pub staged_fallback: bool,
    pub root: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub response: SqliteDiffResponse,
    pub limit: usize,
    pub after: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReadPathContentOptions {
    pub path: PathBuf,
    pub revision: String,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathDiffResult {
    pub path: String,
    pub diff: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffTelemetry {
    pub duration_us: u64,
    pub requested_paths: usize,
    pub returned_paths: usize,
    pub changed_paths: usize,
    pub path_filter_fast_path: bool,
    pub table_filter_fast_path: bool,
    pub requested_table: Option<String>,
    pub tables_scanned: usize,
    pub full_tree_paths_hydrated: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffPathsResult {
    pub paths: Vec<PathDiffResult>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub telemetry: DiffTelemetry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SqliteDiffTelemetry {
    pub duration_us: u64,
    pub requested_paths: usize,
    pub returned_paths: usize,
    pub changed_paths: usize,
    pub response_scope: String,
    pub requested_table: Option<String>,
    pub tables_scanned: usize,
    pub rows_scanned: usize,
    pub rows_returned: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SqliteDiffPathsResult {
    pub paths: Vec<PathDiffResult>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub telemetry: SqliteDiffTelemetry,
}

#[derive(Debug, Clone)]
pub struct RestoreOptions {
    pub source: Option<String>,
    pub expected_head: Option<String>,
    pub require_clean: bool,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct StagePathsOptions {
    pub paths: Vec<PathBuf>,
    pub expected_head: Option<String>,
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct RecordPathMoveOptions {
    pub previous_path: PathBuf,
    pub path: PathBuf,
    pub expected_head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordPathMoveResult {
    pub previous_path: String,
    pub path: String,
    pub change: &'static str,
    pub materializes_worktree: bool,
}

#[derive(Debug, Clone)]
pub struct UntrackPathsOptions {
    pub paths: Vec<PathBuf>,
    pub expected_head: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RestorePathsOptions {
    pub source: Option<String>,
    pub expected_head: Option<String>,
    pub require_clean: bool,
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchPathResult {
    pub path: String,
    pub result: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchPathsResult {
    pub paths: Vec<BatchPathResult>,
    pub materializes_worktree: bool,
}

#[derive(Debug, Clone)]
pub struct RemoteConfigureOptions {
    pub name: String,
    pub url: String,
    pub bearer_token: Option<String>,
    pub overwrite: bool,
    pub upstream_branch: Option<String>,
}

struct SessionState {
    service: Option<RepositoryCommandService>,
    status_cache: IncrementalStatusCache,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileFingerprint {
    is_file: bool,
    len: u64,
    modified_ns: Option<u128>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TrackedFingerprint {
    main: Option<FileFingerprint>,
    wal: Option<FileFingerprint>,
    shm: Option<FileFingerprint>,
    journal: Option<FileFingerprint>,
}

#[derive(Default)]
struct IncrementalStatusCache {
    initialized: bool,
    index_metadata_initialized: bool,
    head_target: Option<String>,
    index: Index,
    files: BTreeMap<String, CommitFileState>,
    artifacts: BTreeMap<String, CommitArtifactState>,
    tracked_fingerprints: BTreeMap<String, TrackedFingerprint>,
    untracked_fingerprints: BTreeMap<String, FileFingerprint>,
    status: Option<RepoStatus>,
    generation: u64,
    ignore_matcher: Option<graft::repo::RepoIgnoreMatcher>,
    tracked_ignored_paths: Option<Vec<String>>,
    persistent_snapshot_attempted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedStatusSnapshot {
    schema_version: u32,
    repository_format_version: u32,
    object_format: String,
    repository_metadata_fingerprint: String,
    ignore_source_fingerprint: String,
    head_target: Option<String>,
    index: Index,
    files: BTreeMap<String, CommitFileState>,
    artifacts: BTreeMap<String, CommitArtifactState>,
    tracked_fingerprints: BTreeMap<String, TrackedFingerprint>,
    untracked_fingerprints: BTreeMap<String, FileFingerprint>,
    status: RepoStatus,
    generation: u64,
}

impl IncrementalStatusCache {
    fn invalidate(&mut self) {
        let generation = self.generation;
        *self = Self::default();
        self.generation = generation;
    }
}

/// One long-lived repository session.
///
/// Every operation locks this session for its full duration. Calls on the same session therefore
/// serialize, while independent session instances can run on different worker threads.
pub struct RepositorySession {
    target: PathBuf,
    credentials: RemoteCredentials,
    lifecycle: AtomicU8,
    state: Mutex<SessionState>,
}

impl std::fmt::Debug for RepositorySession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RepositorySession")
            .field("target", &self.target)
            .field("lifecycle", &self.lifecycle())
            .finish()
    }
}

impl RepositorySession {
    /// Creates a closed session. [`Self::open`] performs the potentially expensive runtime setup.
    pub fn new(target: impl AsRef<Path>) -> Self {
        Self {
            target: repository_session_target(target.as_ref()),
            credentials: RemoteCredentials::explicit(),
            lifecycle: AtomicU8::new(LIFECYCLE_CLOSED),
            state: Mutex::new(SessionState {
                service: None,
                status_cache: IncrementalStatusCache::default(),
            }),
        }
    }

    pub fn target(&self) -> &Path {
        &self.target
    }

    pub fn lifecycle(&self) -> SessionLifecycle {
        lifecycle_from_raw(self.lifecycle.load(Ordering::Acquire))
    }

    /// Opens the retained repository runtime.
    pub fn open(&self) -> Result<()> {
        match self.lifecycle.compare_exchange(
            LIFECYCLE_CLOSED,
            LIFECYCLE_OPENING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(LIFECYCLE_OPEN) => {
                return Err(SdkError::new(
                    SdkErrorCode::SessionAlreadyOpen,
                    "repository session is already open",
                ));
            }
            Err(LIFECYCLE_OPENING) => return Err(session_opening_error()),
            Err(LIFECYCLE_CLOSING) => return Err(session_closing_error()),
            Err(_) => unreachable!("invalid repository session lifecycle"),
        }

        let mut state = self.state.lock();
        let service =
            RepositoryCommandService::open_with_credentials(&self.target, self.credentials.clone())
                .map_err(|error| self.command_error(error));
        let service = match service {
            Ok(service) => service,
            Err(error) => {
                self.lifecycle.store(LIFECYCLE_CLOSED, Ordering::Release);
                return Err(error);
            }
        };

        if self.lifecycle.load(Ordering::Acquire) == LIFECYCLE_CLOSING {
            drop(service);
            self.lifecycle.store(LIFECYCLE_CLOSED, Ordering::Release);
            return Err(session_closing_error());
        }

        state.service = Some(service);
        self.lifecycle.store(LIFECYCLE_OPEN, Ordering::Release);
        Ok(())
    }

    /// Waits for the in-flight operation, releases the retained runtime, and rejects queued work.
    pub fn close(&self) -> Result<()> {
        let previous = self.lifecycle.swap(LIFECYCLE_CLOSING, Ordering::AcqRel);
        if previous == LIFECYCLE_CLOSED {
            self.lifecycle.store(LIFECYCLE_CLOSED, Ordering::Release);
            return Ok(());
        }

        let mut state = self.state.lock();
        state.service = None;
        state.status_cache = IncrementalStatusCache::default();
        self.lifecycle.store(LIFECYCLE_CLOSED, Ordering::Release);
        Ok(())
    }

    /// Closes and reconstructs the runtime from durable repository state.
    pub fn reopen(&self) -> Result<()> {
        self.lifecycle.store(LIFECYCLE_CLOSING, Ordering::Release);
        let mut state = self.state.lock();
        state.service = None;
        state.status_cache = IncrementalStatusCache::default();
        self.lifecycle.store(LIFECYCLE_OPENING, Ordering::Release);

        match RepositoryCommandService::open_with_credentials(
            &self.target,
            self.credentials.clone(),
        ) {
            Ok(service) => {
                state.service = Some(service);
                self.lifecycle.store(LIFECYCLE_OPEN, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                self.lifecycle.store(LIFECYCLE_CLOSED, Ordering::Release);
                Err(self.command_error(error))
            }
        }
    }

    /// Injects or rotates a bearer token in memory. This is allowed while the session is closed.
    pub fn set_http_bearer_token(&self, remote_name: &str, token: String) -> Result<()> {
        self.credentials
            .set_http_bearer_token(remote_name, token)
            .map_err(credential_error)
    }

    pub fn clear_http_bearer_token(&self, remote_name: &str) -> Result<()> {
        self.credentials
            .clear_http_bearer_token(remote_name)
            .map_err(credential_error)
    }

    pub fn init(&self) -> Result<Value> {
        self.execute_json_mutating("json_init", None)
    }

    pub fn status(&self) -> Result<Value> {
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            let incremental = refresh_incremental_status(service, status_cache)?;
            if incremental.status.has_conflicts {
                return execute_json(service, "json_status", None);
            }
            let current_branch = service
                .repository()
                .map_err(repository_command_error)?
                .current_branch()
                .map_err(repo_error)?;
            legacy_status_value(incremental.status, current_branch)
        })
    }

    pub fn status_incremental(&self) -> Result<IncrementalStatusResult> {
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            refresh_incremental_status(service, status_cache)
        })
    }

    /// Reads ref and repository format metadata without classifying the worktree.
    pub fn repository_metadata(&self) -> Result<RepositoryMetadataResult> {
        let started = Instant::now();
        self.with_service(|service| {
            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
            let repo = service.repository().map_err(repository_command_error)?;
            let current_head = repo.head_target().map_err(repo_error)?;
            let current_branch = repo.current_branch().map_err(repo_error)?;
            let upstream = current_branch
                .as_deref()
                .map(|branch| repo.branch_upstream(branch))
                .transpose()
                .map_err(repo_error)?
                .flatten();
            let upstream_target = upstream
                .as_ref()
                .map(|upstream| repo.remote_tracking_ref(&upstream.remote, &upstream.branch))
                .transpose()
                .map_err(repo_error)?
                .flatten();
            let config = repo.config().map_err(repo_error)?;
            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
            Ok(RepositoryMetadataResult {
                current_head,
                current_branch,
                upstream,
                upstream_target,
                repository_format_version: config.core.repository_format_version,
                object_format: config.extensions.object_format,
                telemetry: RepositoryMetadataTelemetry {
                    duration_us: elapsed_us(started),
                    paths_examined: 0,
                },
            })
        })
    }

    /// Returns credential-free remote configuration without classifying the worktree.
    pub fn list_remotes(&self) -> Result<ListRemotesResult> {
        let started = Instant::now();
        self.with_service(|service| {
            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
            let repo = service.repository().map_err(repository_command_error)?;
            let mut remotes = Vec::new();
            for remote in repo.remotes().map_err(repo_error)? {
                graft::repo::cancellation_checkpoint().map_err(repo_error)?;
                let (kind, url) = safe_remote_projection(&remote.config);
                remotes.push(SafeRemoteInfo { name: remote.name, kind, url });
            }
            Ok(ListRemotesResult {
                remotes,
                telemetry: RepositoryMetadataTelemetry {
                    duration_us: elapsed_us(started),
                    paths_examined: 0,
                },
            })
        })
    }

    pub fn add_all(&self) -> Result<Value> {
        self.execute_json_mutating("json_add", Some("--all"))
    }

    /// Stages a bounded path collection under one serialized session operation.
    pub fn stage_paths(&self, options: &StagePathsOptions) -> Result<BatchPathsResult> {
        let paths = normalize_batch_paths(&options.paths)?;
        if let Some(expected_head) = &options.expected_head {
            validate_revision(expected_head)?;
        }
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            if let Some(expected_head) = &options.expected_head {
                let actual = service
                    .repository()
                    .map_err(repository_command_error)?
                    .head_target()
                    .map_err(repo_error)?;
                if actual.as_deref() != Some(expected_head) {
                    return Err(invalid_argument(format!(
                        "cannot stage because HEAD changed: expected {expected_head}, found {}",
                        actual.as_deref().unwrap_or("unborn")
                    )));
                }
            }
            let operation = (|| {
                let status = service.status().map_err(repository_command_error)?;
                let unstaged = status
                    .unstaged_changes
                    .iter()
                    .map(|change| change.path.as_str())
                    .collect::<Vec<_>>();
                let staged = status
                    .staged_changes
                    .iter()
                    .flat_map(|change| {
                        [
                            change.path.as_str(),
                            change.previous_path.as_deref().unwrap_or(""),
                        ]
                    })
                    .collect::<Vec<_>>();
                let mut results = Vec::with_capacity(paths.len());
                for path in paths {
                    graft::repo::cancellation_checkpoint().map_err(repo_error)?;
                    let directory_prefix = format!("{path}/");
                    let has_unstaged = unstaged.iter().any(|candidate| {
                        *candidate == path.as_str() || candidate.starts_with(&directory_prefix)
                    });
                    let is_already_staged = staged.iter().any(|candidate| {
                        *candidate == path.as_str() || candidate.starts_with(&directory_prefix)
                    });
                    if !has_unstaged && is_already_staged {
                        results.push(BatchPathResult {
                            path,
                            result: serde_json::json!({ "already_staged": true }),
                        });
                        continue;
                    }
                    let argument = if options.force {
                        format!("--force -- {}", quote_pragma_path(Path::new(&path))?)
                    } else {
                        format!("-- {}", quote_pragma_path(Path::new(&path))?)
                    };
                    results.push(BatchPathResult {
                        path,
                        result: execute_json(service, "json_add", Some(&argument))?,
                    });
                }
                Ok(BatchPathsResult {
                    paths: results,
                    materializes_worktree: false,
                })
            })();
            status_cache.invalidate();
            operation
        })
    }

    /// Records a completed physical rename without reading the payload. The operation updates the
    /// index atomically and preserves the tracked `SQLite` snapshot or artifact object identity.
    pub fn record_path_move(
        &self,
        options: &RecordPathMoveOptions,
    ) -> Result<RecordPathMoveResult> {
        let previous_path = normalize_requested_path(&options.previous_path)?;
        let path = normalize_requested_path(&options.path)?;
        if let Some(expected_head) = &options.expected_head {
            validate_revision(expected_head)?;
        }
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            let repo = service.repository().map_err(repository_command_error)?;
            if let Some(expected_head) = &options.expected_head {
                let actual = repo.head_target().map_err(repo_error)?;
                if actual.as_deref() != Some(expected_head) {
                    return Err(invalid_argument(format!(
                        "cannot record path move because HEAD changed: expected {expected_head}, found {}",
                        actual.as_deref().unwrap_or("unborn")
                    )));
                }
            }
            let result = repo
                .stage_path_move_keys(&previous_path, &path)
                .map(|_| RecordPathMoveResult {
                    previous_path,
                    path,
                    change: "renamed",
                    materializes_worktree: false,
                })
                .map_err(repo_error);
            status_cache.invalidate();
            result
        })
    }

    /// Removes explicit files from the index without deleting or replacing worktree files.
    pub fn untrack_paths(&self, options: &UntrackPathsOptions) -> Result<BatchPathsResult> {
        let paths = normalize_batch_paths(&options.paths)?;
        if let Some(expected_head) = &options.expected_head {
            validate_revision(expected_head)?;
        }
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            let repo = service.repository().map_err(repository_command_error)?;
            if let Some(expected_head) = &options.expected_head {
                let actual = repo.head_target().map_err(repo_error)?;
                if actual.as_deref() != Some(expected_head) {
                    return Err(invalid_argument(format!(
                        "cannot untrack because HEAD changed: expected {expected_head}, found {}",
                        actual.as_deref().unwrap_or("unborn")
                    )));
                }
            }

            let tracked = repo
                .index_files()
                .map_err(repo_error)?
                .into_keys()
                .chain(repo.index_artifacts().map_err(repo_error)?.into_keys())
                .collect::<std::collections::BTreeSet<_>>();
            for path in &paths {
                graft::repo::cancellation_checkpoint().map_err(repo_error)?;
                let physical = repo.worktree().join(path);
                let is_physical_directory = fs::symlink_metadata(&physical)
                    .is_ok_and(|metadata| metadata.file_type().is_dir());
                let directory_prefix = format!("{path}/");
                let is_tracked_directory = tracked
                    .range(directory_prefix.clone()..)
                    .next()
                    .is_some_and(|candidate| candidate.starts_with(&directory_prefix));
                if is_physical_directory || is_tracked_directory {
                    return Err(invalid_argument(format!(
                        "untrack path `{path}` is a directory; provide explicit tracked file paths"
                    )));
                }
            }

            let operation = (|| {
                let mut results = Vec::with_capacity(paths.len());
                for path in paths {
                    graft::repo::cancellation_checkpoint().map_err(repo_error)?;
                    let argument = format!("--cached -- {}", quote_pragma_path(Path::new(&path))?);
                    results.push(BatchPathResult {
                        path,
                        result: execute_json(service, "json_rm", Some(&argument))?,
                    });
                }
                Ok(BatchPathsResult {
                    paths: results,
                    materializes_worktree: false,
                })
            })();
            status_cache.invalidate();
            operation
        })
    }

    pub fn commit(&self, message: &str) -> Result<Value> {
        if message.trim().is_empty() {
            return Err(invalid_argument("commit message must not be empty"));
        }
        self.execute_json_mutating("json_commit", Some(message))
    }

    pub fn diff(&self, options: &DiffOptions) -> Result<Value> {
        let argument = diff_argument(options)?;
        self.execute_json("json_diff", argument.as_deref())
    }

    /// Computes worktree diffs only for an explicit, bounded page of file paths.
    pub fn diff_paths(&self, options: &DiffPathsOptions) -> Result<DiffPathsResult> {
        if options.paths.is_empty() {
            return Err(invalid_argument("diff paths must not be empty"));
        }
        if options.paths.len() > MAX_DIFF_PATH_REQUEST_SIZE {
            return Err(invalid_argument(format!(
                "diff paths request exceeds {MAX_DIFF_PATH_REQUEST_SIZE} paths"
            )));
        }
        if options.limit == 0 || options.limit > MAX_DIFF_PATH_PAGE_SIZE {
            return Err(invalid_argument(format!(
                "diff path limit must be between 1 and {MAX_DIFF_PATH_PAGE_SIZE}"
            )));
        }
        diff_argument(&DiffOptions {
            rows: options.rows,
            root: options.root.clone(),
            from: options.from.clone(),
            to: options.to.clone(),
            path: options.paths.first().cloned(),
            table: options.table.clone(),
            ..DiffOptions::default()
        })?;

        let started = Instant::now();
        let mut paths = options
            .paths
            .iter()
            .map(|path| normalize_requested_path(path))
            .collect::<Result<Vec<_>>>()?;
        paths.sort();
        paths.dedup();
        if let Some(after) = &options.after {
            paths.retain(|path| path > after);
        }
        let requested_paths = paths.len();
        let has_more = paths.len() > options.limit;
        paths.truncate(options.limit);

        let mut results = Vec::with_capacity(paths.len());
        for path in paths {
            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
            let diff = self.diff(&DiffOptions {
                rows: options.rows,
                root: options.root.clone(),
                from: options.from.clone(),
                to: options.to.clone(),
                path: Some(PathBuf::from(&path)),
                table: options.table.clone(),
                ..DiffOptions::default()
            })?;
            results.push(PathDiffResult { path, diff });
        }
        let changed_paths = results
            .iter()
            .filter(|entry| value_changed_path_count(&entry.diff) > 0)
            .count();
        let tables_scanned = results
            .iter()
            .map(|entry| value_row_diff_tables_scanned(&entry.diff))
            .sum();
        let next_cursor = results.last().map(|entry| entry.path.clone());
        Ok(DiffPathsResult {
            telemetry: DiffTelemetry {
                duration_us: elapsed_us(started),
                requested_paths,
                returned_paths: results.len(),
                changed_paths,
                path_filter_fast_path: true,
                table_filter_fast_path: options.table.is_some(),
                requested_table: options.table.clone(),
                tables_scanned,
                full_tree_paths_hydrated: 0,
            },
            paths: results,
            has_more,
            next_cursor,
        })
    }

    /// Computes a SQLite-aware summary or one bounded row page for explicit paths.
    pub fn diff_sqlite_paths(
        &self,
        options: &SqliteDiffPathsOptions,
    ) -> Result<SqliteDiffPathsResult> {
        validate_diff_path_page(&options.paths, options.limit)?;
        let started = Instant::now();
        let mut paths = options
            .paths
            .iter()
            .map(|path| normalize_requested_path(path))
            .collect::<Result<Vec<_>>>()?;
        paths.sort();
        paths.dedup();
        if let Some(after) = &options.after {
            paths.retain(|path| path > after);
        }
        let requested_paths = paths.len();
        let has_more = paths.len() > options.limit;
        paths.truncate(options.limit);
        let mut results = Vec::with_capacity(paths.len());
        for path in paths {
            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
            let argument = sqlite_diff_argument(options, Path::new(&path))?;
            let mut diff = self.execute_json("json_diff", Some(&argument))?;
            if options.staged_fallback
                && !options.staged
                && options.root.is_none()
                && options.from.is_none()
                && options.to.is_none()
            {
                let mut staged_options = options.clone();
                staged_options.staged = true;
                staged_options.staged_fallback = false;
                let worktree_has_changes = value_changed_path_count(&diff) > 0;
                if worktree_has_changes {
                    staged_options.response = SqliteDiffResponse::Summary;
                }
                let staged_argument = sqlite_diff_argument(&staged_options, Path::new(&path))?;
                let staged_diff = self.execute_json("json_diff", Some(&staged_argument))?;
                if worktree_has_changes {
                    overlay_staged_renames(&mut diff, &staged_diff);
                } else {
                    diff = staged_diff;
                }
            }
            results.push(PathDiffResult { path, diff });
        }
        let changed_paths = results
            .iter()
            .filter(|entry| value_changed_path_count(&entry.diff) > 0)
            .count();
        let telemetry = bounded_diff_telemetry(&results);
        let requested_table = match &options.response {
            SqliteDiffResponse::Summary => None,
            SqliteDiffResponse::Rows { table, .. } => Some(table.clone()),
        };
        let next_cursor = results.last().map(|entry| entry.path.clone());
        Ok(SqliteDiffPathsResult {
            telemetry: SqliteDiffTelemetry {
                duration_us: elapsed_us(started),
                requested_paths,
                returned_paths: results.len(),
                changed_paths,
                response_scope: telemetry.response_scope,
                requested_table,
                tables_scanned: telemetry.tables_scanned,
                rows_scanned: telemetry.rows_scanned,
                rows_returned: telemetry.rows_returned,
                truncated: telemetry.truncated,
            },
            paths: results,
            has_more,
            next_cursor,
        })
    }

    /// Reads bounded UTF-8 artifact content for one explicit path at an immutable revision.
    pub fn read_path_content(&self, options: &ReadPathContentOptions) -> Result<RepoPathContent> {
        if options.max_bytes == 0 || options.max_bytes > MAX_PATH_CONTENT_BYTES {
            return Err(invalid_argument(format!(
                "path content max_bytes must be between 1 and {MAX_PATH_CONTENT_BYTES}"
            )));
        }
        let path = normalize_requested_path(&options.path)?;
        validate_revision(&options.revision)?;

        self.with_service(|service| {
            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
            let repo = service.repository().map_err(repository_command_error)?;
            let content = repo
                .read_path_content(&options.revision, &path, ByteUnit::new(options.max_bytes))
                .map_err(repo_error)?;
            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
            Ok(content)
        })
    }

    pub fn history(&self, limit: usize, after: Option<&str>) -> Result<Value> {
        if limit == 0 {
            return Err(invalid_argument("history limit must be greater than zero"));
        }
        let mut argument = format!("--with-status --limit {limit}");
        if let Some(after) = after {
            validate_revision(after)?;
            argument.push_str(" --after ");
            argument.push_str(after);
        }
        self.execute_json("json_log", Some(&argument))
    }

    /// Returns a bounded summary page. Commit trees and blobs are never read by this operation.
    pub fn history_summaries(
        &self,
        limit: usize,
        after: Option<&str>,
    ) -> Result<HistorySummariesResult> {
        if limit == 0 || limit > MAX_HISTORY_SUMMARY_PAGE_SIZE {
            return Err(invalid_argument(format!(
                "history summary limit must be between 1 and {MAX_HISTORY_SUMMARY_PAGE_SIZE}"
            )));
        }
        if let Some(after) = after {
            validate_revision(after)?;
        }
        let started = Instant::now();
        self.with_service(|service| {
            let page = service
                .history_summaries(limit, after)
                .map_err(repository_command_error)?;
            let commits_returned = page.commits.len();
            Ok(HistorySummariesResult {
                commits: page.commits,
                has_more: page.has_more,
                next_cursor: page.next_cursor,
                telemetry: HistoryTelemetry {
                    duration_us: elapsed_us(started),
                    commits_returned,
                    tree_objects_read: 0,
                    blob_objects_read: 0,
                },
            })
        })
    }

    /// Lazily loads the full tree-backed commit payload for one revision.
    pub fn commit_details(&self, revision: &str) -> Result<Value> {
        validate_revision(revision)?;
        self.with_service(|service| {
            let commit = service
                .commit_details(revision)
                .map_err(repository_command_error)?;
            serde_json::to_value(commit).map_err(status_encode_error)
        })
    }

    /// Lazily hydrates one commit and returns a bounded first-parent changed-path page.
    pub fn commit_changed_paths(
        &self,
        options: &CommitChangedPathsOptions,
    ) -> Result<CommitChangedPathsResult> {
        validate_revision(&options.revision)?;
        if options.limit == 0 || options.limit > MAX_COMMIT_CHANGED_PATH_PAGE_SIZE {
            return Err(invalid_argument(format!(
                "commit changed path limit must be between 1 and {MAX_COMMIT_CHANGED_PATH_PAGE_SIZE}"
            )));
        }
        let after = options
            .after
            .as_deref()
            .map(|path| normalize_requested_path(Path::new(path)))
            .transpose()?;
        let started = Instant::now();
        self.with_service(|service| {
            let page = service
                .commit_changed_paths(&options.revision, options.limit, after.as_deref())
                .map_err(repository_command_error)?;
            let tree_objects_read = if page.parent.is_some() { 2 } else { 1 };
            let items_returned = page.paths.len();
            Ok(CommitChangedPathsResult {
                revision: page.revision,
                parent: page.parent,
                paths: page.paths,
                total_changed_paths: page.total_changed_paths,
                has_more: page.has_more,
                next_cursor: page.next_cursor,
                telemetry: CommitChangedPathsTelemetry {
                    duration_us: elapsed_us(started),
                    paths_examined: page.total_changed_paths,
                    items_returned,
                    tree_objects_read,
                    blob_objects_read: 0,
                },
            })
        })
    }

    /// Evaluates one path with Graft's nested `.gitignore` and `.graftignore` semantics.
    pub fn is_ignored_path(&self, path: &Path) -> Result<IgnoredPathResult> {
        self.is_ignored_paths(&IgnoredPathsOptions { paths: vec![path.to_path_buf()] })?
            .paths
            .into_iter()
            .next()
            .ok_or_else(|| invalid_argument("ignore path query must not be empty"))
    }

    /// Evaluates a bounded path collection with shared ignore and tracked-index caches.
    pub fn is_ignored_paths(&self, options: &IgnoredPathsOptions) -> Result<IgnoredPathsResult> {
        let paths = normalize_ignore_query_paths(&options.paths)?;
        let started = Instant::now();
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            let index_cache_hit = ensure_index_metadata(service, status_cache)?;
            let repo = service.repository().map_err(repository_command_error)?;
            let ignore_matcher_cache_hit = ensure_ignore_matcher(&repo, status_cache)?;
            let files = &status_cache.files;
            let artifacts = &status_cache.artifacts;
            let matcher = status_cache
                .ignore_matcher
                .as_mut()
                .expect("ignore matcher cache was initialized");
            let mut results = Vec::with_capacity(paths.len());
            for path in paths {
                graft::repo::cancellation_checkpoint().map_err(repo_error)?;
                let has_tracked_descendants =
                    cached_path_has_tracked_descendants(files, artifacts, &path);
                let is_directory = has_tracked_descendants
                    || fs::symlink_metadata(repo.worktree().join(&path))
                        .is_ok_and(|metadata| metadata.file_type().is_dir());
                results.push(IgnoredPathResult {
                    is_ignored: matcher
                        .is_ignored(&path, is_directory)
                        .map_err(repo_error)?,
                    is_tracked: cached_path_is_tracked(files, artifacts, &path),
                    is_directory,
                    has_tracked_descendants,
                    path,
                });
            }
            let paths_examined = results.len();
            Ok(IgnoredPathsResult {
                paths: results,
                telemetry: IgnoredPathsTelemetry {
                    duration_us: elapsed_us(started),
                    paths_examined,
                    index_cache_hit,
                    ignore_matcher_cache_hit,
                },
            })
        })
    }

    /// Returns one bounded inventory page. Ignored scans reuse one nested-rule matcher.
    pub fn inventory(&self, options: &InventoryOptions) -> Result<InventoryResult> {
        if options.limit == 0 || options.limit > MAX_INVENTORY_PAGE_SIZE {
            return Err(invalid_argument(format!(
                "inventory limit must be between 1 and {MAX_INVENTORY_PAGE_SIZE}"
            )));
        }
        let started = Instant::now();
        self.with_state(|state| {
            let SessionState {
                service,
                status_cache,
            } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            let index_cache_hit = if options.kind == InventoryKind::Untracked {
                refresh_incremental_status(service, status_cache)?
                    .telemetry
                    .status_cache_hit
            } else {
                ensure_index_metadata(service, status_cache)?
            };
            let repo = service.repository().map_err(repository_command_error)?;
            let ignore_matcher_cache_hit = ensure_ignore_matcher(&repo, status_cache)?;
            let mut tracked = status_cache
                .files
                .keys()
                .chain(status_cache.artifacts.keys())
                .cloned()
                .collect::<Vec<_>>();
            tracked.sort();
            tracked.dedup();
            let mut paths_examined = 0;
            let mut inventory_cache_hit = false;
            let mut candidates = match options.kind {
                InventoryKind::Tracked => {
                    paths_examined = tracked.len();
                    tracked.clone()
                }
                InventoryKind::Untracked => {
                    let paths = status_cache.untracked_fingerprints.keys().cloned().collect::<Vec<_>>();
                    paths_examined = paths.len();
                    paths
                }
                InventoryKind::TrackedIgnored => {
                    if let Some(paths) = &status_cache.tracked_ignored_paths {
                        inventory_cache_hit = true;
                        paths.clone()
                    } else {
                        let matcher = status_cache
                            .ignore_matcher
                            .as_mut()
                            .expect("ignore matcher cache was initialized");
                        let mut paths = Vec::new();
                        for path in &tracked {
                            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
                            paths_examined += 1;
                            if matcher.is_ignored(path, false).map_err(repo_error)? {
                                paths.push(path.clone());
                            }
                        }
                        status_cache.tracked_ignored_paths = Some(paths.clone());
                        paths
                    }
                }
                InventoryKind::Ignored => {
                    let mut paths = Vec::new();
                    let matcher = status_cache
                        .ignore_matcher
                        .as_mut()
                        .expect("ignore matcher cache was initialized");
                    collect_ignored_files(
                        &repo,
                        matcher,
                        repo.worktree(),
                        false,
                        &mut paths,
                        &mut paths_examined,
                    )?;
                    paths
                }
            };
            candidates.sort();
            candidates.dedup();
            let total_matching = candidates.len();
            if let Some(after) = &options.after {
                candidates.retain(|path| path > after);
            }
            let has_more = candidates.len() > options.limit;
            candidates.truncate(options.limit);
            let mut items = Vec::with_capacity(candidates.len());
            for path in candidates {
                graft::repo::cancellation_checkpoint().map_err(repo_error)?;
                let ignored = match options.kind {
                    InventoryKind::Ignored | InventoryKind::TrackedIgnored => true,
                    InventoryKind::Untracked => false,
                    InventoryKind::Tracked => status_cache
                        .ignore_matcher
                        .as_mut()
                        .expect("ignore matcher cache was initialized")
                        .is_ignored(&path, false)
                        .map_err(repo_error)?,
                };
                items.push(InventoryItem {
                    tracked: cached_path_is_tracked(
                        &status_cache.files,
                        &status_cache.artifacts,
                        &path,
                    ),
                    path,
                    ignored,
                });
            }
            let next_cursor = items.last().map(|item| item.path.clone());
            let migration = (options.kind == InventoryKind::TrackedIgnored).then(|| {
                IgnoreMigrationDiagnostic {
                    ignored_rules_do_not_untrack: true,
                    tracked_ignored_paths: total_matching,
                    recommendation: "Remove paths from the index explicitly after reviewing this page; adding an ignore rule never untracks existing paths.".to_string(),
                }
            });
            let items_returned = items.len();
            Ok(InventoryResult {
                kind: options.kind,
                items,
                total_matching,
                has_more,
                next_cursor,
                migration,
                telemetry: InventoryTelemetry {
                    duration_us: elapsed_us(started),
                    paths_examined,
                    items_returned,
                    inventory_cache_hit,
                    index_cache_hit,
                    ignore_matcher_cache_hit,
                },
            })
        })
    }

    pub fn restore(&self, options: &RestoreOptions) -> Result<Value> {
        let mut parts = Vec::new();
        if let Some(source) = &options.source {
            validate_revision(source)?;
            parts.push(format!("--source {source}"));
        }
        if let Some(expected_head) = &options.expected_head {
            validate_revision(expected_head)?;
            parts.push(format!("--expected-head {expected_head}"));
        }
        if options.require_clean {
            parts.push("--require-clean".to_string());
        }
        parts.push("--".to_string());
        parts.push(quote_pragma_path(&options.path)?);
        self.execute_json_mutating("json_restore", Some(&parts.join(" ")))
    }

    /// Restores a bounded path collection under one serialized session operation.
    pub fn restore_paths(&self, options: &RestorePathsOptions) -> Result<BatchPathsResult> {
        let paths = normalize_batch_paths(&options.paths)?;
        if let Some(source) = &options.source {
            validate_revision(source)?;
        }
        if let Some(expected_head) = &options.expected_head {
            validate_revision(expected_head)?;
        }
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            let operation = (|| {
                let mut results = Vec::with_capacity(paths.len());
                for path in paths {
                    graft::repo::cancellation_checkpoint().map_err(repo_error)?;
                    let mut parts = Vec::new();
                    if let Some(source) = &options.source {
                        parts.push(format!("--source {source}"));
                    }
                    if let Some(expected_head) = &options.expected_head {
                        parts.push(format!("--expected-head {expected_head}"));
                    }
                    if options.require_clean {
                        parts.push("--require-clean".to_string());
                    }
                    parts.push("--".to_string());
                    parts.push(quote_pragma_path(Path::new(&path))?);
                    results.push(BatchPathResult {
                        path,
                        result: execute_json(service, "json_restore", Some(&parts.join(" ")))?,
                    });
                }
                Ok(BatchPathsResult {
                    paths: results,
                    materializes_worktree: true,
                })
            })();
            status_cache.invalidate();
            operation
        })
    }

    pub fn configure_remote(&self, options: &RemoteConfigureOptions) -> Result<Value> {
        validate_remote_name(&options.name)?;
        validate_sdk_remote_url(&options.url)?;
        if let Some(token) = &options.bearer_token {
            self.set_http_bearer_token(&options.name, token.clone())?;
        }

        self.with_service(|service| {
            let remotes = execute_json(service, "json_remotes", None)?;
            let existing_url = remote_url(&remotes, &options.name)?;
            match existing_url {
                None => {
                    let argument = format!("{} {}", options.name, options.url);
                    execute_json(service, "json_remote_add", Some(&argument))?;
                }
                Some(existing) if existing == options.url => {}
                Some(_) if options.overwrite => {
                    let argument = format!("{} {}", options.name, options.url);
                    execute_json(service, "json_remote_set_url", Some(&argument))?;
                }
                Some(_) => {
                    return Err(SdkError::new(
                        SdkErrorCode::InvalidArgument,
                        format!(
                            "remote `{}` already exists with a different URL",
                            options.name
                        ),
                    ));
                }
            }

            if let Some(branch) = &options.upstream_branch {
                validate_branch_name(branch)?;
                let argument = format!("{branch} {}/{branch}", options.name);
                execute_json(service, "json_branch_upstream", Some(&argument))?;
            }
            execute_json(service, "json_remotes", None)
        })
    }

    pub fn push(&self, remote: Option<&str>, branch: Option<&str>) -> Result<Value> {
        let argument = remote_branch_argument(remote, branch)?;
        self.execute_json("json_push", argument.as_deref())
    }

    pub fn fetch(&self, remote: Option<&str>, branch: Option<&str>) -> Result<Value> {
        let argument = remote_branch_argument(remote, branch)?;
        // Fetch updates objects and remote-tracking refs, not the local worktree. Preserve the
        // proven local classification and refresh only the repository projection on next status.
        self.execute_json("json_fetch", argument.as_deref())
    }

    pub fn pull(&self, remote: Option<&str>, branch: Option<&str>) -> Result<Value> {
        let argument = remote_branch_argument(remote, branch)?;
        self.execute_json_mutating("json_pull", argument.as_deref())
    }

    /// Returns the effective merge policy and its stable compare-and-swap token.
    pub fn get_merge_policy(&self) -> Result<MergePolicyResult> {
        self.with_service(|service| {
            merge_policy_result(service.merge_policy().map_err(repository_command_error)?)
        })
    }

    /// Validates and normalizes a policy document without changing repository state.
    pub fn validate_merge_policy(
        &self,
        policy: &MergePolicyDocument,
    ) -> MergePolicyValidationResult {
        if policy.version != graft::repo::MERGE_POLICY_VERSION {
            return MergePolicyValidationResult {
                valid: false,
                policy: None,
                policy_token: None,
                errors: vec![MergePolicyValidationIssue {
                    key: "version".to_string(),
                    value: policy.version.to_string(),
                    message: format!(
                        "expected merge policy version {}",
                        graft::repo::MERGE_POLICY_VERSION
                    ),
                }],
            };
        }
        if let Err(error) = policy.config.validate() {
            let issue = match error {
                graft::repo::RepoErr::InvalidConfigValue { key, value, message } => {
                    MergePolicyValidationIssue { key, value, message }
                }
                other => MergePolicyValidationIssue {
                    key: "policy".to_string(),
                    value: String::new(),
                    message: other.to_string(),
                },
            };
            return MergePolicyValidationResult {
                valid: false,
                policy: None,
                policy_token: None,
                errors: vec![issue],
            };
        }
        let config = policy.config.effective();
        let policy_token = config.policy_token();
        MergePolicyValidationResult {
            valid: true,
            policy: Some(MergePolicyDocument {
                version: graft::repo::MERGE_POLICY_VERSION,
                config,
            }),
            policy_token: Some(policy_token),
            errors: Vec::new(),
        }
    }

    /// Replaces the merge policy under a policy-token CAS guard.
    pub fn set_merge_policy(&self, options: &SetMergePolicyOptions) -> Result<MergePolicyResult> {
        if options.expected_policy_token.trim().is_empty() {
            return Err(invalid_argument("expected policy token must not be empty"));
        }
        let validation = self.validate_merge_policy(&options.policy);
        let Some(policy) = validation.policy else {
            let message = validation.errors.first().map_or_else(
                || "invalid merge policy".to_string(),
                |issue| format!("{}: {}", issue.key, issue.message),
            );
            return Err(invalid_argument(message));
        };
        self.with_service(|service| {
            let observed = service.merge_policy().map_err(repository_command_error)?;
            if observed.active_merge {
                return Err(repository_stale_error(
                    "merge policy is frozen during an active merge; abort or finish the merge first",
                ));
            }
            if observed.token != options.expected_policy_token {
                return Err(repository_stale_error(
                    "merge policy changed; read the policy again",
                ));
            }
            let repo = service.repository().map_err(repository_command_error)?;
            let mut config = repo.config().map_err(repo_error)?;
            config.merge = policy.config;
            repo.write_config(&config).map_err(repo_error)?;
            merge_policy_result(service.merge_policy().map_err(repository_command_error)?)
        })
    }

    /// Computes merge topology and path conflicts without changing refs, index, or worktree.
    pub fn plan_merge(&self, options: &PlanMergeOptions) -> Result<MergePlanResult> {
        validate_revision(&options.revision)?;
        if let Some(expected_head) = &options.expected_head {
            validate_revision(expected_head)?;
        }
        self.with_service(|service| {
            let policy = service.merge_policy().map_err(repository_command_error)?;
            let repo = service.repository().map_err(repository_command_error)?;
            ensure_expected_head(&repo, options.expected_head.as_deref())?;
            let plan = repo
                .plan_merge_revision(&options.revision)
                .map_err(repo_error)?;
            merge_plan_result(&plan, &policy.token, policy.version)
        })
    }

    /// Applies a previously reviewed merge plan under HEAD and plan-token compare-and-swap guards.
    pub fn apply_merge(&self, options: &ApplyMergeOptions) -> Result<MergeApplyResult> {
        validate_revision(&options.revision)?;
        if let Some(expected_head) = &options.expected_head {
            validate_revision(expected_head)?;
        }
        if options.plan_token.trim().is_empty() {
            return Err(invalid_argument("merge plan token must not be empty"));
        }
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            let policy = service.merge_policy().map_err(repository_command_error)?;
            let repo = service.repository().map_err(repository_command_error)?;
            ensure_expected_head(&repo, options.expected_head.as_deref())?;
            let plan = repo
                .plan_merge_revision(&options.revision)
                .map_err(repo_error)?;
            let summary = merge_plan_result(&plan, &policy.token, policy.version)?;
            if summary.plan_token != options.plan_token {
                return Err(repository_stale_error(
                    "merge plan changed; plan the merge again",
                ));
            }
            let output = execute_json_command(
                service,
                RepositoryCommand::merge(options.revision.clone()),
                "merge",
            )?;
            status_cache.invalidate();
            let incremental = refresh_incremental_status(service, status_cache)?;
            let merge = merge_status_from_incremental(service, &incremental)?;
            let worktree_paths = merge_worktree_paths_from_output(&output);
            Ok(MergeApplyResult {
                plan: summary,
                output,
                merge,
                worktree_paths,
            })
        })
    }

    /// Reconstructs the singleton merge state from durable refs and index stages.
    pub fn get_merge_status(&self) -> Result<MergeStatus> {
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            let incremental = refresh_incremental_status(service, status_cache)?;
            merge_status_from_incremental(service, &incremental)
        })
    }

    /// Lists merge paths in stable path order without running `SQLite` row analysis.
    pub fn list_merge_paths(&self, options: &ListMergePathsOptions) -> Result<MergePathPage> {
        validate_page_limit(options.limit, MAX_MERGE_PATH_PAGE_SIZE, "merge path")?;
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            let incremental =
                require_merge_state_token(service, status_cache, &options.expected_state_token)?;
            let repo = service.repository().map_err(repository_command_error)?;
            let index = repo.read_index().map_err(repo_error)?;
            let state_token = durable_merge_state_token(&repo, &incremental.status, &index)?;
            let mut items = merge_paths_for_index(&index)
                .into_iter()
                .filter(|item| match options.filter {
                    MergePathFilter::All => true,
                    MergePathFilter::Unmerged => item.state == MergePathState::Unmerged,
                    MergePathFilter::Resolved => item.state == MergePathState::Resolved,
                })
                .collect::<Vec<_>>();
            let start = page_start_for_cursor(
                &items,
                options.after.as_deref(),
                |item| item.path.clone(),
                "merge path",
            )?;
            let has_more = items.len().saturating_sub(start) > options.limit;
            items = items.into_iter().skip(start).take(options.limit).collect();
            let next_cursor = has_more
                .then(|| items.last().map(|item| item.path.clone()))
                .flatten();
            Ok(MergePathPage { state_token, items, next_cursor })
        })
    }

    /// Returns one bounded conflict page for a selected path.
    pub fn list_merge_conflicts(
        &self,
        options: &ListMergeConflictsOptions,
    ) -> Result<MergeConflictPage> {
        validate_page_limit(
            options.limit,
            MAX_MERGE_CONFLICT_PAGE_SIZE,
            "merge conflict",
        )?;
        let path = normalize_requested_path(&options.path)?;
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            let incremental =
                require_merge_state_token(service, status_cache, &options.expected_state_token)?;
            let repo = service.repository().map_err(repository_command_error)?;
            let index = repo.read_index().map_err(repo_error)?;
            let state_token = durable_merge_state_token(&repo, &incremental.status, &index)?;
            let output =
                execute_json_command(service, RepositoryCommand::conflicts(), "conflicts")?;
            let conflicts = output
                .get("conflicts")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    SdkError::new(
                        SdkErrorCode::InvalidResponse,
                        "conflicts response did not contain a conflict array",
                    )
                })?;
            let mut items = conflicts
                .iter()
                .filter(|item| item.get("path").and_then(Value::as_str) == Some(path.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            items.sort_by(|left, right| conflict_id(left).cmp(conflict_id(right)));
            let start = page_start_for_cursor(
                &items,
                options.after.as_deref(),
                |item| conflict_id(item).to_string(),
                "merge conflict",
            )?;
            let has_more = items.len().saturating_sub(start) > options.limit;
            items = items.into_iter().skip(start).take(options.limit).collect();
            let next_cursor = has_more
                .then(|| items.last().map(|item| conflict_id(item).to_string()))
                .flatten();
            Ok(MergeConflictPage { state_token, path, items, next_cursor })
        })
    }

    /// Reads a bounded Base/Ours/Theirs revision or the current editable worktree result.
    pub fn read_merge_version(&self, options: &ReadMergeVersionOptions) -> Result<MergeContent> {
        if options.max_bytes == 0 || options.max_bytes > MAX_PATH_CONTENT_BYTES {
            return Err(invalid_argument(format!(
                "merge content max_bytes must be between 1 and {MAX_PATH_CONTENT_BYTES}"
            )));
        }
        let path = normalize_requested_path(&options.path)?;
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            let incremental =
                require_merge_state_token(service, status_cache, &options.expected_state_token)?;
            let repo = service.repository().map_err(repository_command_error)?;
            let index = repo.read_index().map_err(repo_error)?;
            let state_token = durable_merge_state_token(&repo, &incremental.status, &index)?;
            let (orig_head, merge_head, merge_base) = active_merge_heads(&repo, &incremental)?;
            if options.version == MergeVersion::Result {
                return read_worktree_merge_content(&repo, &path, options.max_bytes, state_token);
            }
            let (label, revision) = match options.version {
                MergeVersion::Base => ("base", merge_base),
                MergeVersion::Ours => ("ours", Some(orig_head)),
                MergeVersion::Theirs => ("theirs", Some(merge_head)),
                MergeVersion::Result => unreachable!("handled above"),
            };
            let Some(revision) = revision else {
                return Ok(MergeContent {
                    version: label.to_string(),
                    revision: None,
                    path,
                    kind: None,
                    storage: None,
                    content: MergeContentState::Absent,
                    state_token,
                });
            };
            let content = repo
                .read_path_content(&revision, &path, ByteUnit::new(options.max_bytes))
                .map_err(repo_error)?;
            Ok(project_merge_content(
                label,
                Some(revision),
                content,
                state_token,
            ))
        })
    }

    /// Returns a bounded, read-only `SQLite` diff between two immutable active-merge versions.
    pub fn diff_merge_sqlite(
        &self,
        options: &DiffMergeSqliteOptions,
    ) -> Result<MergeSqliteDiffResult> {
        if options.from == options.to {
            return Err(invalid_argument(
                "merge SQLite diff versions must be different",
            ));
        }
        let path = normalize_requested_path(&options.path)?;
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            let incremental =
                require_merge_state_token(service, status_cache, &options.expected_state_token)?;
            let repo = service.repository().map_err(repository_command_error)?;
            let index = repo.read_index().map_err(repo_error)?;
            let state_token = durable_merge_state_token(&repo, &incremental.status, &index)?;
            let (orig_head, merge_head, merge_base) = active_merge_heads(&repo, &incremental)?;
            let revision_for = |version: MergeSqliteVersion| -> Result<String> {
                match version {
                    MergeSqliteVersion::Base => merge_base.clone().ok_or_else(|| {
                        SdkError::new(
                            SdkErrorCode::RepositoryCommand,
                            "active merge has no common base for SQLite inspection",
                        )
                    }),
                    MergeSqliteVersion::Ours => Ok(orig_head.clone()),
                    MergeSqliteVersion::Theirs => Ok(merge_head.clone()),
                }
            };
            let from_revision = revision_for(options.from)?;
            let to_revision = revision_for(options.to)?;
            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
            let argument = sqlite_diff_argument(
                &SqliteDiffPathsOptions {
                    paths: vec![PathBuf::from(&path)],
                    staged: false,
                    staged_fallback: false,
                    root: None,
                    from: Some(from_revision.clone()),
                    to: Some(to_revision.clone()),
                    response: options.response.clone(),
                    limit: 1,
                    after: None,
                },
                Path::new(&path),
            )?;
            let diff = execute_json(service, "json_diff", Some(&argument))?;
            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
            Ok(MergeSqliteDiffResult {
                state_token,
                path,
                from: MergeSqliteDiffEndpoint {
                    version: options.from,
                    revision: from_revision,
                },
                to: MergeSqliteDiffEndpoint {
                    version: options.to,
                    revision: to_revision,
                },
                diff,
            })
        })
    }

    /// Selects stage 2 or stage 3 for one conflicted path and collapses it to stage 0.
    pub fn set_merge_path_result(
        &self,
        options: &SetMergePathResultOptions,
    ) -> Result<MergeOperationResult> {
        let path = normalize_requested_path(&options.path)?;
        self.mutate_merge_with_resolution(
            &path,
            None,
            options.result,
            &options.expected_state_token,
        )
    }

    /// Selects ours or theirs for one `SQLite` row conflict.
    pub fn resolve_merge_row(
        &self,
        options: &ResolveMergeRowOptions,
    ) -> Result<MergeOperationResult> {
        if options.table.trim().is_empty() {
            return Err(invalid_argument("merge row table must not be empty"));
        }
        if !matches!(options.identity, Value::Number(_) | Value::Object(_)) {
            return Err(invalid_argument(
                "merge row identity must be a JSON number or object",
            ));
        }
        let path = normalize_requested_path(&options.path)?;
        self.mutate_merge_with_resolution(
            &path,
            Some(ServiceResolveRow {
                table: options.table.clone(),
                identity: options.identity.clone(),
            }),
            options.result,
            &options.expected_state_token,
        )
    }

    /// Selects ours or theirs for one structured `SQLite` cell conflict.
    pub fn resolve_merge_cell(
        &self,
        options: &ResolveMergeCellOptions,
    ) -> Result<MergeOperationResult> {
        if options.table.trim().is_empty() || options.column.trim().is_empty() {
            return Err(invalid_argument(
                "merge cell table and column must not be empty",
            ));
        }
        if !matches!(options.identity, Value::Number(_) | Value::Object(_)) {
            return Err(invalid_argument(
                "merge cell identity must be a JSON number or object",
            ));
        }
        let path = normalize_requested_path(&options.path)?;
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            require_merge_state_token(service, status_cache, &options.expected_state_token)?;
            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
            let side = match options.result {
                MergePathResult::Ours => ServiceResolveSide::Ours,
                MergePathResult::Theirs => ServiceResolveSide::Theirs,
            };
            let resolved = service
                .resolve_cell(ServiceResolveCellOptions {
                    side,
                    path: PathBuf::from(&path),
                    table: options.table.clone(),
                    identity: options.identity.clone(),
                    column: options.column.clone(),
                })
                .map_err(repository_command_error)?;
            status_cache.invalidate();
            let incremental = refresh_incremental_status(service, status_cache)?;
            let merge = merge_status_from_incremental(service, &incremental)?;
            Ok(MergeOperationResult {
                output: serde_json::json!({
                    "operation": "resolve_merge_cell",
                    "path": resolved.path,
                    "table": options.table,
                    "column": options.column,
                    "materialized": resolved.materialized,
                    "resolution": match options.result {
                        MergePathResult::Ours => "ours",
                        MergePathResult::Theirs => "theirs",
                    },
                }),
                merge,
                worktree_paths: if resolved.materialized {
                    vec![resolved.path]
                } else {
                    Vec::new()
                },
            })
        })
    }

    /// Integrity-checks and stages the current `SQLite` worktree candidate.
    pub fn stage_merge_sqlite_result(
        &self,
        options: &StageMergeSqliteResultOptions,
    ) -> Result<MergeOperationResult> {
        let path = normalize_requested_path(&options.path)?;
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            require_merge_state_token(service, status_cache, &options.expected_state_token)?;
            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
            let staged_path = service
                .stage_worktree_sqlite_result(Path::new(&path))
                .map_err(repository_command_error)?;
            status_cache.invalidate();
            let incremental = refresh_incremental_status(service, status_cache)?;
            let merge = merge_status_from_incremental(service, &incremental)?;
            Ok(MergeOperationResult {
                output: serde_json::json!({
                    "operation": "stage_merge_sqlite_result",
                    "path": staged_path,
                    "resolution": "edited",
                    "integrity_check": "ok",
                    "foreign_key_check": "ok",
                    "materialized": false,
                }),
                merge,
                worktree_paths: Vec::new(),
            })
        })
    }

    /// Creates or reopens a durable private Base/Ours/Theirs workspace for an application merge
    /// provider. Preparing the workspace does not change the index, merge journal, or worktree.
    pub fn prepare_semantic_merge(
        &self,
        options: &PrepareSemanticMergeOptions,
    ) -> Result<SemanticMergeWorkspace> {
        validate_semantic_provider_name(&options.provider)?;
        let managed_tables = validate_semantic_managed_tables(&options.managed_tables)?;
        let managed_table_set = managed_tables.iter().cloned().collect::<BTreeSet<_>>();
        let path = normalize_requested_path(&options.path)?;
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            let incremental =
                require_merge_state_token(service, status_cache, &options.expected_state_token)?;
            let repo = service.repository().map_err(repository_command_error)?;
            let index = repo.read_index().map_err(repo_error)?;
            let merge_path = merge_paths_for_index(&index)
                .into_iter()
                .find(|item| item.path == path)
                .ok_or_else(|| {
                    invalid_argument(format!("path `{path}` is not part of the active merge"))
                })?;
            if merge_path.state != MergePathState::Unmerged
                || merge_path.kind != RepoTrackedPathKind::SqliteDatabase
            {
                return Err(invalid_argument(format!(
                    "path `{path}` is not an unresolved SQLite merge path"
                )));
            }
            let state_token = durable_merge_state_token(&repo, &incremental.status, &index)?;
            let policy = service.merge_policy().map_err(repository_command_error)?;
            let (orig_head, merge_head, merge_base) = active_merge_heads(&repo, &incremental)?;
            let provider_token = semantic_merge_provider_token(
                &options.provider,
                &path,
                &state_token,
                &policy.token,
                policy.version,
                &orig_head,
                &merge_head,
                merge_base.as_deref(),
                &managed_tables,
            )?;
            let workspace_path = semantic_merge_workspace_path(&repo, &provider_token)?;
            let manifest_path = workspace_path.join("manifest.json");
            if manifest_path.is_file() {
                let manifest = read_semantic_merge_manifest(&manifest_path)?;
                validate_semantic_merge_workspace_paths(&manifest, &workspace_path)?;
                validate_semantic_merge_manifest(
                    &manifest,
                    &provider_token,
                    &options.provider,
                    &path,
                    &state_token,
                    &managed_tables,
                )?;
                return Ok(manifest.workspace);
            }
            if workspace_path.exists() {
                return Err(SdkError::new(
                    SdkErrorCode::InvalidResponse,
                    "semantic merge workspace exists without a valid manifest",
                ));
            }
            fs::create_dir_all(&workspace_path)
                .map_err(|error| repository_command_error(ErrCtx::IoErr(error)))?;

            let creation = (|| -> Result<SemanticMergeWorkspace> {
                let mut inputs = Vec::with_capacity(3);
                for (version, revision, file_name) in [
                    (
                        MergeSqliteVersion::Base,
                        merge_base.as_deref(),
                        "base.sqlite",
                    ),
                    (
                        MergeSqliteVersion::Ours,
                        Some(orig_head.as_str()),
                        "ours.sqlite",
                    ),
                    (
                        MergeSqliteVersion::Theirs,
                        Some(merge_head.as_str()),
                        "theirs.sqlite",
                    ),
                ] {
                    inputs.push(export_semantic_merge_input(
                        service,
                        &path,
                        version,
                        revision,
                        &workspace_path.join(file_name),
                    )?);
                }
                let result_path = workspace_path.join("result.sqlite");
                let seed = service
                    .prepare_semantic_merge_seed(Path::new(&path), &result_path, &managed_table_set)
                    .map_err(repository_command_error)?;
                let workspace = SemanticMergeWorkspace {
                    provider_token: provider_token.clone(),
                    provider: options.provider.clone(),
                    path: path.clone(),
                    workspace_path: workspace_path.to_string_lossy().into_owned(),
                    result_path: result_path.to_string_lossy().into_owned(),
                    managed_tables: managed_tables.clone(),
                    seed_applied_sql: seed.applied_sql,
                    managed_conflicts: seed.managed_conflicts,
                    prepared_at_unix_ms: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_err(|_| invalid_argument("system clock is before the Unix epoch"))?
                        .as_millis()
                        .try_into()
                        .map_err(|_| {
                            invalid_argument("system clock exceeds the supported range")
                        })?,
                    state_token,
                    policy_token: policy.token,
                    policy_version: policy.version,
                    orig_head,
                    merge_head,
                    merge_base,
                    inputs,
                    record: SemanticMergeProviderRecord::Pending,
                };
                write_semantic_merge_manifest(
                    &manifest_path,
                    &SemanticMergeWorkspaceManifest {
                        version: SEMANTIC_MERGE_WORKSPACE_VERSION,
                        workspace: workspace.clone(),
                    },
                )?;
                Ok(workspace)
            })();
            if creation.is_err() {
                let _ = fs::remove_dir_all(&workspace_path);
            }
            creation
        })
    }

    /// Persists application-domain conflicts without resolving the underlying Graft path.
    pub fn record_semantic_merge_conflicts(
        &self,
        options: &RecordSemanticMergeConflictsOptions,
    ) -> Result<SemanticMergeWorkspace> {
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            require_merge_state_token(service, status_cache, &options.expected_state_token)?;
            let repo = service.repository().map_err(repository_command_error)?;
            let workspace_path = semantic_merge_workspace_path(&repo, &options.provider_token)?;
            let manifest_path = workspace_path.join("manifest.json");
            let mut manifest = read_semantic_merge_manifest(&manifest_path)?;
            validate_semantic_merge_workspace_paths(&manifest, &workspace_path)?;
            validate_semantic_merge_record_state(
                &manifest,
                &options.provider_token,
                &options.expected_state_token,
            )?;
            manifest.workspace.record = SemanticMergeProviderRecord::Conflict {
                conflicts: options.conflicts.clone(),
                automatic_resolutions: options.automatic_resolutions.clone(),
            };
            ensure_semantic_merge_record_bounded(&manifest.workspace.record)?;
            write_semantic_merge_manifest(&manifest_path, &manifest)?;
            Ok(manifest.workspace)
        })
    }

    /// Accepts the fixed provider result, validates it as `SQLite`, materializes it through the
    /// normal checkout boundary, and stages the path under the current merge-state token.
    pub fn accept_semantic_merge_result(
        &self,
        options: &AcceptSemanticMergeResultOptions,
    ) -> Result<MergeOperationResult> {
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            require_merge_state_token(service, status_cache, &options.expected_state_token)?;
            let repo = service.repository().map_err(repository_command_error)?;
            let workspace_path = semantic_merge_workspace_path(&repo, &options.provider_token)?;
            let manifest_path = workspace_path.join("manifest.json");
            let mut manifest = read_semantic_merge_manifest(&manifest_path)?;
            validate_semantic_merge_workspace_paths(&manifest, &workspace_path)?;
            validate_semantic_merge_record_state(
                &manifest,
                &options.provider_token,
                &options.expected_state_token,
            )?;
            let record = SemanticMergeProviderRecord::Merged {
                validation: options.validation.clone(),
                automatic_resolutions: options.automatic_resolutions.clone(),
            };
            ensure_semantic_merge_record_bounded(&record)?;
            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
            let previous_record = manifest.workspace.record.clone();
            manifest.workspace.record = record;
            // Persist the audit record first. A crash after this point remains
            // retryable because the underlying conflict stages are still
            // present until the materialization succeeds.
            write_semantic_merge_manifest(&manifest_path, &manifest)?;
            let staged_path = match service.stage_external_sqlite_result(
                Path::new(&manifest.workspace.path),
                Path::new(&manifest.workspace.result_path),
            ) {
                Ok(staged_path) => staged_path,
                Err(error) => {
                    manifest.workspace.record = previous_record;
                    let _ = write_semantic_merge_manifest(&manifest_path, &manifest);
                    return Err(repository_command_error(error));
                }
            };
            status_cache.invalidate();
            let incremental = refresh_incremental_status(service, status_cache)?;
            let merge = merge_status_from_incremental(service, &incremental)?;
            Ok(MergeOperationResult {
                output: serde_json::json!({
                    "operation": "accept_semantic_merge_result",
                    "path": staged_path,
                    "provider": manifest.workspace.provider,
                    "provider_token": manifest.workspace.provider_token,
                    "resolution": "semantic_provider",
                    "integrity_check": "ok",
                    "foreign_key_check": "ok",
                    "materialized": true,
                }),
                merge,
                worktree_paths: vec![staged_path],
            })
        })
    }

    /// Atomically selects one side for every safely row-resolvable conflict in a `SQLite` table.
    pub fn resolve_merge_table(
        &self,
        options: &ResolveMergeTableOptions,
    ) -> Result<MergeOperationResult> {
        if options.table.trim().is_empty() {
            return Err(invalid_argument("merge table must not be empty"));
        }
        let path = normalize_requested_path(&options.path)?;
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            require_merge_state_token(service, status_cache, &options.expected_state_token)?;
            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
            let side = match options.result {
                MergePathResult::Ours => ServiceResolveSide::Ours,
                MergePathResult::Theirs => ServiceResolveSide::Theirs,
            };
            let command = RepositoryCommand::resolve_table(ServiceResolveTableOptions {
                side,
                path: PathBuf::from(&path),
                table: options.table.clone(),
            });
            let output = execute_json_command(service, command, "resolve_merge_table")?;
            status_cache.invalidate();
            let incremental = refresh_incremental_status(service, status_cache)?;
            let merge = merge_status_from_incremental(service, &incremental)?;
            let worktree_paths = merge_worktree_paths_from_output(&output);
            Ok(MergeOperationResult { output, merge, worktree_paths })
        })
    }

    /// Restores a resolved path to its original Base/Ours/Theirs merge stages and worktree state.
    pub fn unresolve_merge_path(
        &self,
        options: &UnresolveMergePathOptions,
    ) -> Result<MergeOperationResult> {
        let path = normalize_requested_path(&options.path)?;
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            require_merge_state_token(service, status_cache, &options.expected_state_token)?;
            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
            let output = execute_json_command(
                service,
                RepositoryCommand::unresolve(PathBuf::from(&path)),
                "unresolve_merge_path",
            )?;
            status_cache.invalidate();
            let incremental = refresh_incremental_status(service, status_cache)?;
            let merge = merge_status_from_incremental(service, &incremental)?;
            let worktree_paths = merge_worktree_paths_from_output(&output);
            Ok(MergeOperationResult { output, merge, worktree_paths })
        })
    }

    /// Writes an edited UTF-8 result and stages it as the complete resolution for one text path.
    pub fn write_and_stage_text_result(
        &self,
        options: &WriteAndStageTextResultOptions,
    ) -> Result<MergeOperationResult> {
        if options.content.len() as u64 > MAX_PATH_CONTENT_BYTES {
            return Err(invalid_argument(format!(
                "edited merge result exceeds {MAX_PATH_CONTENT_BYTES} bytes"
            )));
        }
        let path = normalize_requested_path(&options.path)?;
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            require_merge_state_token(service, status_cache, &options.expected_state_token)?;
            let repo = service.repository().map_err(repository_command_error)?;
            ensure_text_merge_conflict(&repo, &path)?;
            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
            let physical_path = repo.worktree().join(&path);
            write_merge_text_result(&repo, &physical_path, options.content.as_bytes())?;
            let entry = repo
                .resolve_artifact_conflict_from_path(&physical_path)
                .map_err(repo_error)?;
            execute_json_command(
                service,
                RepositoryCommand::record_merge_path_resolution(PathBuf::from(&path), "edited"),
                "record_merge_path_resolution",
            )?;
            status_cache.invalidate();
            let incremental = refresh_incremental_status(service, status_cache)?;
            let merge = merge_status_from_incremental(service, &incremental)?;
            Ok(MergeOperationResult {
                output: serde_json::json!({
                    "operation": "write_and_stage_text_result",
                    "path": entry.path,
                    "resolution": "edited",
                    "materialized": true,
                }),
                merge,
                worktree_paths: vec![entry.path],
            })
        })
    }

    /// Completes the current merge only if the candidate still matches the validated token.
    pub fn continue_merge(&self, options: &ContinueMergeOptions) -> Result<MergeOperationResult> {
        if options.message.trim().is_empty() || options.message.contains('\0') {
            return Err(invalid_argument(
                "merge commit message must not be empty or contain NUL",
            ));
        }
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            require_merge_state_token(service, status_cache, &options.expected_state_token)?;
            let semantic_workspaces = service
                .repository()
                .map_err(repository_command_error)?
                .graft_dir()
                .join(SEMANTIC_MERGE_WORKSPACE_DIRECTORY);
            let output = execute_json_command(
                service,
                RepositoryCommand::merge_continue(options.message.clone()),
                "merge_continue",
            )?;
            status_cache.invalidate();
            let incremental = refresh_incremental_status(service, status_cache)?;
            let merge = merge_status_from_incremental(service, &incremental)?;
            let worktree_paths = merge_worktree_paths_from_output(&output);
            let _ = fs::remove_dir_all(semantic_workspaces);
            Ok(MergeOperationResult { output, merge, worktree_paths })
        })
    }

    /// Aborts the current merge only if the merge state still matches the caller's token.
    pub fn abort_merge(&self, options: &AbortMergeOptions) -> Result<MergeOperationResult> {
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            require_merge_state_token(service, status_cache, &options.expected_state_token)?;
            let semantic_workspaces = service
                .repository()
                .map_err(repository_command_error)?
                .graft_dir()
                .join(SEMANTIC_MERGE_WORKSPACE_DIRECTORY);
            let output =
                execute_json_command(service, RepositoryCommand::merge_abort(), "merge_abort")?;
            status_cache.invalidate();
            let incremental = refresh_incremental_status(service, status_cache)?;
            let merge = merge_status_from_incremental(service, &incremental)?;
            let worktree_paths = merge_worktree_paths_from_output(&output);
            let _ = fs::remove_dir_all(semantic_workspaces);
            Ok(MergeOperationResult { output, merge, worktree_paths })
        })
    }

    fn mutate_merge_with_resolution(
        &self,
        path: &str,
        row: Option<ServiceResolveRow>,
        result: MergePathResult,
        expected_state_token: &str,
    ) -> Result<MergeOperationResult> {
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            require_merge_state_token(service, status_cache, expected_state_token)?;
            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
            let side = match result {
                MergePathResult::Ours => ServiceResolveSide::Ours,
                MergePathResult::Theirs => ServiceResolveSide::Theirs,
            };
            let command = RepositoryCommand::resolve(ServiceResolveOptions {
                side,
                path: Some(PathBuf::from(path)),
                row,
            })
            .map_err(repository_command_error)?;
            let output = execute_json_command(service, command, "resolve_conflict")?;
            status_cache.invalidate();
            let incremental = refresh_incremental_status(service, status_cache)?;
            let merge = merge_status_from_incremental(service, &incremental)?;
            let worktree_paths = merge_worktree_paths_from_output(&output);
            Ok(MergeOperationResult { output, merge, worktree_paths })
        })
    }

    pub fn clone_repository(
        &self,
        remote_url: &str,
        branch: Option<&str>,
        bearer_token: Option<String>,
    ) -> Result<Value> {
        validate_sdk_remote_url(remote_url)?;
        if let Some(token) = bearer_token {
            self.set_http_bearer_token("origin", token)?;
        }
        let argument = match branch {
            Some(branch) => {
                validate_branch_name(branch)?;
                format!("{remote_url} {branch}")
            }
            None => remote_url.to_string(),
        };
        self.execute_json_mutating("json_clone", Some(&argument))
    }

    fn execute_json(&self, name: &str, argument: Option<&str>) -> Result<Value> {
        self.with_service(|service| execute_json(service, name, argument))
    }

    fn execute_json_mutating(&self, name: &str, argument: Option<&str>) -> Result<Value> {
        self.with_state(|state| {
            let SessionState { service, status_cache } = state;
            let service = service.as_mut().ok_or_else(session_closed_error)?;
            let result = execute_json(service, name, argument);
            status_cache.invalidate();
            result
        })
    }

    fn with_service<T>(
        &self,
        operation: impl FnOnce(&mut RepositoryCommandService) -> Result<T>,
    ) -> Result<T> {
        self.with_state(|state| {
            let service = state.service.as_mut().ok_or_else(session_closed_error)?;
            operation(service)
        })
    }

    fn with_state<T>(&self, operation: impl FnOnce(&mut SessionState) -> Result<T>) -> Result<T> {
        match self.lifecycle.load(Ordering::Acquire) {
            LIFECYCLE_CLOSED => return Err(session_closed_error()),
            LIFECYCLE_OPENING => return Err(session_opening_error()),
            LIFECYCLE_CLOSING => return Err(session_closing_error()),
            LIFECYCLE_OPEN => {}
            _ => unreachable!("invalid repository session lifecycle"),
        }

        let mut state = self.state.lock();
        if self.lifecycle.load(Ordering::Acquire) != LIFECYCLE_OPEN {
            return Err(session_closing_error());
        }
        operation(&mut state).map_err(|error| self.redact_error(error))
    }

    fn command_error(&self, error: ErrCtx) -> SdkError {
        let message = self.credentials.redact(&error.to_string());
        let code = sdk_error_code_for_error(&error, &message);
        SdkError::new(code, message)
    }

    fn redact_error(&self, error: SdkError) -> SdkError {
        SdkError::new(error.code, self.credentials.redact(&error.message))
    }
}

impl Drop for RepositorySession {
    fn drop(&mut self) {
        self.lifecycle.store(LIFECYCLE_CLOSING, Ordering::Release);
        self.state.get_mut().service = None;
        self.state.get_mut().status_cache = IncrementalStatusCache::default();
        self.lifecycle.store(LIFECYCLE_CLOSED, Ordering::Release);
    }
}

fn ensure_index_metadata(
    service: &mut RepositoryCommandService,
    cache: &mut IncrementalStatusCache,
) -> Result<bool> {
    let repo = service.repository().map_err(repository_command_error)?;
    let head_target = repo.head_target().map_err(repo_error)?;
    let index = repo.read_index().map_err(repo_error)?;
    if cache.index_metadata_initialized && cache.head_target == head_target && cache.index == index
    {
        return Ok(true);
    }
    cache.files = repo.index_files().map_err(repo_error)?;
    cache.artifacts = repo.index_artifacts().map_err(repo_error)?;
    cache.head_target = head_target;
    cache.index = index;
    cache.index_metadata_initialized = true;
    cache.tracked_ignored_paths = None;
    Ok(false)
}

fn ensure_ignore_matcher(repo: &Repository, cache: &mut IncrementalStatusCache) -> Result<bool> {
    let cache_hit = match cache.ignore_matcher.as_ref() {
        Some(matcher) => matcher.rules_unchanged().map_err(repo_error)?,
        None => false,
    };
    if cache_hit {
        return Ok(true);
    }
    cache.ignore_matcher = Some(repo.ignore_matcher().map_err(repo_error)?);
    cache.tracked_ignored_paths = None;
    Ok(false)
}

fn refresh_incremental_status(
    service: &mut RepositoryCommandService,
    cache: &mut IncrementalStatusCache,
) -> Result<IncrementalStatusResult> {
    for attempt in 0..WORKTREE_STABILITY_ATTEMPTS {
        match refresh_incremental_status_once(service, cache) {
            Ok(mut result) => {
                result.telemetry.stability_retries = attempt;
                return Ok(result);
            }
            Err(error)
                if error.code() == SdkErrorCode::RepositoryStale
                    && attempt + 1 < WORKTREE_STABILITY_ATTEMPTS =>
            {
                cache.invalidate();
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("bounded status stability loop always returns")
}

fn refresh_incremental_status_once(
    service: &mut RepositoryCommandService,
    cache: &mut IncrementalStatusCache,
) -> Result<IncrementalStatusResult> {
    let started = Instant::now();
    let repo = service.repository().map_err(repository_command_error)?;
    let head_target = repo.head_target().map_err(repo_error)?;
    let index = repo.read_index().map_err(repo_error)?;
    if index.has_conflicts() {
        return refresh_conflicted_incremental_status(service, cache, started, head_target, index);
    }
    let persistent_snapshot_hit = if cache.persistent_snapshot_attempted {
        false
    } else {
        cache.persistent_snapshot_attempted = true;
        match load_persistent_status_snapshot(&repo, cache, &head_target, &index) {
            Ok(hit) => hit,
            Err(error) => {
                cache.persistent_snapshot_attempted = false;
                return Err(error);
            }
        }
    };
    let index_changed = !cache.index_metadata_initialized || cache.index != index;
    if index_changed {
        cache.tracked_ignored_paths = None;
    }
    let tree_cache_hit = cache.initialized && cache.head_target == head_target;
    let same_repository_state = cache.initialized && tree_cache_hit && cache.index == index;

    if same_repository_state {
        let (tracked, untracked) = if persistent_snapshot_hit {
            stable_worktree_fingerprints(&repo, &cache.files, &cache.artifacts)?
        } else {
            worktree_fingerprints(&repo, &cache.files, &cache.artifacts)?
        };
        let metadata_cache_hits = matching_fingerprint_count(&cache.tracked_fingerprints, &tracked)
            + matching_fingerprint_count(&cache.untracked_fingerprints, &untracked);
        let paths_examined = tracked.len() + untracked.len();
        if tracked == cache.tracked_fingerprints && untracked == cache.untracked_fingerprints {
            let previous_status = cache
                .status
                .clone()
                .expect("initialized status cache contains a status");
            let mut status = previous_status.clone();
            repo.refresh_status_repository_projection(&mut status)
                .map_err(repo_error)?;
            let projection_changed = status_changed(Some(&previous_status), &status)?;
            if projection_changed {
                cache.generation = cache.generation.saturating_add(1).max(1);
                cache.status = Some(status.clone());
            }
            let persistent_snapshot_saved = if projection_changed {
                match persist_status_snapshot(&repo, cache) {
                    Ok(saved) => saved,
                    Err(error) if error.code() == SdkErrorCode::Cancelled => return Err(error),
                    Err(_) => false,
                }
            } else {
                false
            };
            return Ok(incremental_status_result(
                cache,
                status,
                started,
                StatusTelemetry {
                    duration_us: 0,
                    paths_examined,
                    metadata_cache_hits,
                    metadata_cache_misses: 0,
                    tree_cache_hit,
                    status_cache_hit: true,
                    persistent_snapshot_hit,
                    persistent_snapshot_saved,
                    stability_retries: 0,
                },
            ));
        }
    }

    let previous_status = cache.status.clone();
    let before_files = repo.index_files().map_err(repo_error)?;
    let before_artifacts = repo.index_artifacts().map_err(repo_error)?;
    let (before_tracked, before_untracked) =
        worktree_fingerprints(&repo, &before_files, &before_artifacts)?;
    let status = service.status().map_err(repository_command_error)?;
    let after_head = repo.head_target().map_err(repo_error)?;
    let after_index = repo.read_index().map_err(repo_error)?;
    if after_head != head_target || after_index != index {
        return Err(repository_stale_error(
            "repository refs or index changed while status was being collected",
        ));
    }
    cache.files = repo.index_files().map_err(repo_error)?;
    cache.artifacts = repo.index_artifacts().map_err(repo_error)?;
    let (tracked, untracked) = stable_worktree_fingerprints(&repo, &cache.files, &cache.artifacts)?;
    if before_files != cache.files
        || before_artifacts != cache.artifacts
        || !worktree_fingerprint_shapes_equal(
            &before_tracked,
            &before_untracked,
            &tracked,
            &untracked,
        )
    {
        return Err(repository_stale_error(
            "worktree changed while status was being collected",
        ));
    }
    cache.tracked_fingerprints = tracked;
    cache.untracked_fingerprints = untracked;
    cache.head_target = head_target;
    cache.index = index;
    cache.index_metadata_initialized = true;
    cache.initialized = true;
    if status_changed(previous_status.as_ref(), &status)? {
        cache.generation = cache.generation.saturating_add(1).max(1);
    }
    cache.status = Some(status.clone());
    let paths_examined = cache.tracked_fingerprints.len() + cache.untracked_fingerprints.len();
    let persistent_snapshot_saved = match persist_status_snapshot(&repo, cache) {
        Ok(saved) => saved,
        Err(error) if error.code() == SdkErrorCode::Cancelled => return Err(error),
        Err(_) => false,
    };
    Ok(incremental_status_result(
        cache,
        status,
        started,
        StatusTelemetry {
            duration_us: 0,
            paths_examined,
            metadata_cache_hits: 0,
            metadata_cache_misses: paths_examined,
            tree_cache_hit,
            status_cache_hit: false,
            persistent_snapshot_hit: false,
            persistent_snapshot_saved,
            stability_retries: 0,
        },
    ))
}

fn refresh_conflicted_incremental_status(
    service: &mut RepositoryCommandService,
    cache: &mut IncrementalStatusCache,
    started: Instant,
    head_target: Option<String>,
    index: Index,
) -> Result<IncrementalStatusResult> {
    let previous_status = cache.status.clone();
    let status = service.status().map_err(repository_command_error)?;
    let repo = service.repository().map_err(repository_command_error)?;
    if repo.head_target().map_err(repo_error)? != head_target
        || repo.read_index().map_err(repo_error)? != index
    {
        return Err(repository_stale_error(
            "repository refs or conflict index changed while status was being collected",
        ));
    }
    cache.files.clear();
    cache.artifacts.clear();
    cache.tracked_fingerprints.clear();
    cache.untracked_fingerprints.clear();
    cache.head_target = head_target;
    cache.index = index;
    cache.index_metadata_initialized = true;
    cache.initialized = true;
    cache.persistent_snapshot_attempted = false;
    if status_changed(previous_status.as_ref(), &status)? {
        cache.generation = cache.generation.saturating_add(1).max(1);
    }
    cache.status = Some(status.clone());
    Ok(incremental_status_result(
        cache,
        status,
        started,
        StatusTelemetry {
            duration_us: 0,
            paths_examined: cache.index.entries.len(),
            metadata_cache_hits: 0,
            metadata_cache_misses: cache.index.entries.len(),
            tree_cache_hit: false,
            status_cache_hit: false,
            persistent_snapshot_hit: false,
            persistent_snapshot_saved: false,
            stability_retries: 0,
        },
    ))
}

fn incremental_status_result(
    cache: &IncrementalStatusCache,
    status: RepoStatus,
    started: Instant,
    mut telemetry: StatusTelemetry,
) -> IncrementalStatusResult {
    telemetry.duration_us = elapsed_us(started);
    let head = cache.head_target.as_deref().unwrap_or("unborn");
    let status_digest = serde_json::to_vec(&status).map_or_else(
        |_| "unavailable".to_string(),
        |bytes| blake3::hash(&bytes).to_hex().to_string(),
    );
    IncrementalStatusResult {
        generation: cache.generation,
        change_token: format!("{head}:{}:{status_digest}", cache.generation),
        status,
        telemetry,
    }
}

fn legacy_status_value(status: RepoStatus, current_branch: Option<String>) -> Result<Value> {
    let current_head = status.head_target.clone();
    let mut value = serde_json::to_value(status).map_err(status_encode_error)?;
    let object = value.as_object_mut().ok_or_else(|| {
        SdkError::new(
            SdkErrorCode::InvalidResponse,
            "repository status did not encode as an object",
        )
    })?;
    if let Some(current_head) = current_head {
        object.insert("current_head".to_string(), Value::String(current_head));
    }
    if let Some(current_branch) = current_branch {
        object.insert("current_branch".to_string(), Value::String(current_branch));
    }
    Ok(value)
}

fn stable_worktree_fingerprints(
    repo: &Repository,
    files: &BTreeMap<String, CommitFileState>,
    artifacts: &BTreeMap<String, CommitArtifactState>,
) -> Result<(
    BTreeMap<String, TrackedFingerprint>,
    BTreeMap<String, FileFingerprint>,
)> {
    let first = worktree_fingerprints(repo, files, artifacts)?;
    let second = worktree_fingerprints(repo, files, artifacts)?;
    if !worktree_fingerprint_shapes_equal(&first.0, &first.1, &second.0, &second.1) {
        return Err(repository_stale_error(
            "worktree changed while path metadata was being sampled",
        ));
    }
    Ok(second)
}

fn worktree_fingerprint_shapes_equal(
    first_tracked: &BTreeMap<String, TrackedFingerprint>,
    first_untracked: &BTreeMap<String, FileFingerprint>,
    second_tracked: &BTreeMap<String, TrackedFingerprint>,
    second_untracked: &BTreeMap<String, FileFingerprint>,
) -> bool {
    first_tracked.len() == second_tracked.len()
        && first_tracked.iter().all(|(key, first)| {
            second_tracked.get(key).is_some_and(|second| {
                optional_fingerprint_shape_equal(&first.main, &second.main)
                    && optional_fingerprint_shape_equal(&first.wal, &second.wal)
                    && optional_fingerprint_shape_equal(&first.shm, &second.shm)
                    && optional_fingerprint_shape_equal(&first.journal, &second.journal)
            })
        })
        && first_untracked.len() == second_untracked.len()
        && first_untracked.iter().all(|(key, first)| {
            second_untracked
                .get(key)
                .is_some_and(|second| first.is_file == second.is_file)
        })
}

fn optional_fingerprint_shape_equal(
    first: &Option<FileFingerprint>,
    second: &Option<FileFingerprint>,
) -> bool {
    match (first, second) {
        (None, None) => true,
        (Some(first), Some(second)) => first.is_file == second.is_file,
        _ => false,
    }
}

fn worktree_fingerprints(
    repo: &Repository,
    files: &BTreeMap<String, CommitFileState>,
    artifacts: &BTreeMap<String, CommitArtifactState>,
) -> Result<(
    BTreeMap<String, TrackedFingerprint>,
    BTreeMap<String, FileFingerprint>,
)> {
    Ok((
        tracked_fingerprints(repo, files, artifacts)?,
        visible_untracked_fingerprints(repo, files, artifacts)?,
    ))
}

fn tracked_fingerprints(
    repo: &Repository,
    files: &BTreeMap<String, CommitFileState>,
    artifacts: &BTreeMap<String, CommitArtifactState>,
) -> Result<BTreeMap<String, TrackedFingerprint>> {
    let mut fingerprints = BTreeMap::new();
    for key in files.keys() {
        graft::repo::cancellation_checkpoint().map_err(repo_error)?;
        fingerprints.insert(
            key.clone(),
            tracked_fingerprint(repo.worktree().join(key), true)?,
        );
    }
    for key in artifacts.keys() {
        graft::repo::cancellation_checkpoint().map_err(repo_error)?;
        fingerprints.insert(
            key.clone(),
            tracked_fingerprint(repo.worktree().join(key), false)?,
        );
    }
    Ok(fingerprints)
}

fn tracked_fingerprint(path: PathBuf, sqlite: bool) -> Result<TrackedFingerprint> {
    let main = fingerprint_path(&path)?;
    let sidecar = |suffix: &str| {
        fingerprint_path(PathBuf::from(format!(
            "{}{}",
            path.to_string_lossy(),
            suffix
        )))
    };
    Ok(TrackedFingerprint {
        main,
        wal: sqlite.then(|| sidecar("-wal")).transpose()?.flatten(),
        shm: sqlite.then(|| sidecar("-shm")).transpose()?.flatten(),
        journal: sqlite.then(|| sidecar("-journal")).transpose()?.flatten(),
    })
}

fn fingerprint_path(path: impl AsRef<Path>) -> Result<Option<FileFingerprint>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            #[cfg(unix)]
            use std::os::unix::fs::MetadataExt;

            Ok(Some(FileFingerprint {
                is_file: metadata.file_type().is_file(),
                len: metadata.len(),
                modified_ns: metadata.modified().ok().and_then(system_time_ns),
                #[cfg(unix)]
                device: metadata.dev(),
                #[cfg(unix)]
                inode: metadata.ino(),
                #[cfg(unix)]
                changed_seconds: metadata.ctime(),
                #[cfg(unix)]
                changed_nanoseconds: metadata.ctime_nsec(),
            }))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(repository_command_error(error.into())),
    }
}

fn system_time_ns(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|value| value.as_nanos())
}

fn load_persistent_status_snapshot(
    repo: &Repository,
    cache: &mut IncrementalStatusCache,
    head_target: &Option<String>,
    index: &Index,
) -> Result<bool> {
    graft::repo::cancellation_checkpoint().map_err(repo_error)?;
    let repository_metadata_fingerprint = repository_metadata_fingerprint(repo, index)?;
    let ignore_source_fingerprint = ignore_source_fingerprint(repo)?;
    let directory = status_snapshot_directory(repo);
    let mut candidates = match fs::read_dir(&directory) {
        Ok(entries) => entries
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                (name.starts_with("classification-v1-") && name.ends_with(".json"))
                    .then(|| entry.path())
            })
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Ok(false),
    };
    candidates.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH)
    });
    for path in candidates.into_iter().rev().take(MAX_STATUS_SNAPSHOTS) {
        graft::repo::cancellation_checkpoint().map_err(repo_error)?;
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if metadata.len() > STATUS_SNAPSHOT_MAX_BYTES {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        if !snapshot_filename_matches(&path, &bytes) {
            continue;
        }
        let Ok(mut snapshot) = serde_json::from_slice::<PersistedStatusSnapshot>(&bytes) else {
            continue;
        };
        if snapshot.schema_version != STATUS_SNAPSHOT_SCHEMA_VERSION
            || snapshot.repository_format_version != graft::repo::REPOSITORY_FORMAT_VERSION
            || snapshot.object_format != graft::repo::OBJECT_FORMAT
            || snapshot.repository_metadata_fingerprint != repository_metadata_fingerprint
            || snapshot.ignore_source_fingerprint != ignore_source_fingerprint
            || &snapshot.head_target != head_target
            || &snapshot.index != index
        {
            continue;
        }
        snapshot.status.worktree = repo.worktree().to_path_buf();
        snapshot.status.graft_dir = repo.graft_dir().to_path_buf();
        cache.initialized = true;
        cache.index_metadata_initialized = true;
        cache.head_target = snapshot.head_target;
        cache.index = snapshot.index;
        cache.files = snapshot.files;
        cache.artifacts = snapshot.artifacts;
        cache.tracked_fingerprints = snapshot.tracked_fingerprints;
        cache.untracked_fingerprints = snapshot.untracked_fingerprints;
        cache.status = Some(snapshot.status);
        cache.generation = snapshot.generation;
        cache.ignore_matcher = None;
        cache.tracked_ignored_paths = None;
        return Ok(true);
    }
    Ok(false)
}

fn persist_status_snapshot(repo: &Repository, cache: &IncrementalStatusCache) -> Result<bool> {
    graft::repo::cancellation_checkpoint().map_err(repo_error)?;
    let Some(status) = cache.status.as_ref() else {
        return Ok(false);
    };
    let mut status = status.clone();
    status.worktree = PathBuf::new();
    status.graft_dir = PathBuf::new();
    let snapshot = PersistedStatusSnapshot {
        schema_version: STATUS_SNAPSHOT_SCHEMA_VERSION,
        repository_format_version: graft::repo::REPOSITORY_FORMAT_VERSION,
        object_format: graft::repo::OBJECT_FORMAT.to_string(),
        repository_metadata_fingerprint: repository_metadata_fingerprint(repo, &cache.index)?,
        ignore_source_fingerprint: ignore_source_fingerprint(repo)?,
        head_target: cache.head_target.clone(),
        index: cache.index.clone(),
        files: cache.files.clone(),
        artifacts: cache.artifacts.clone(),
        tracked_fingerprints: cache.tracked_fingerprints.clone(),
        untracked_fingerprints: cache.untracked_fingerprints.clone(),
        status,
        generation: cache.generation,
    };
    let bytes = serde_json::to_vec(&snapshot).map_err(status_encode_error)?;
    if bytes.len() as u64 > STATUS_SNAPSHOT_MAX_BYTES {
        return Ok(false);
    }
    graft::repo::cancellation_checkpoint().map_err(repo_error)?;
    let directory = status_snapshot_directory(repo);
    fs::create_dir_all(&directory).map_err(|error| repository_command_error(error.into()))?;
    let digest = blake3::hash(&bytes).to_hex().to_string();
    let final_path = directory.join(format!("classification-v1-{digest}.json"));
    if final_path.exists() {
        return Ok(true);
    }
    for attempt in 0..100 {
        let tmp_path = directory.join(format!(
            ".classification-v1-{}-{}-{attempt}.tmp",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        let mut file = match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(repository_command_error(error.into())),
        };
        let write_result = (|| -> std::io::Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&tmp_path);
            return Err(repository_command_error(error.into()));
        }
        if let Err(error) = graft::repo::cancellation_checkpoint() {
            let _ = fs::remove_file(&tmp_path);
            return Err(repo_error(error));
        }
        match fs::rename(&tmp_path, &final_path) {
            Ok(()) => {}
            Err(_error) if final_path.exists() => {
                let _ = fs::remove_file(&tmp_path);
            }
            Err(error) => {
                let _ = fs::remove_file(&tmp_path);
                return Err(repository_command_error(error.into()));
            }
        }
        let _ = fs::File::open(&directory).and_then(|directory| directory.sync_all());
        prune_status_snapshots(&directory, &final_path);
        return Ok(true);
    }
    Ok(false)
}

fn status_snapshot_directory(repo: &Repository) -> PathBuf {
    repo.graft_dir().join("cache").join("sdk-status")
}

fn snapshot_filename_matches(path: &Path, bytes: &[u8]) -> bool {
    let expected = format!("classification-v1-{}.json", blake3::hash(bytes).to_hex());
    path.file_name()
        .is_some_and(|name| name == expected.as_str())
}

fn prune_status_snapshots(directory: &Path, keep: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut snapshots = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with("classification-v1-") && name.ends_with(".json")
            })
        })
        .collect::<Vec<_>>();
    snapshots.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH)
    });
    let remove_count = snapshots.len().saturating_sub(MAX_STATUS_SNAPSHOTS);
    for path in snapshots.into_iter().take(remove_count) {
        if path != keep {
            let _ = fs::remove_file(path);
        }
    }
}

fn repository_metadata_fingerprint(repo: &Repository, index: &Index) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&STATUS_SNAPSHOT_SCHEMA_VERSION.to_le_bytes());
    hasher.update(&graft::repo::REPOSITORY_FORMAT_VERSION.to_le_bytes());
    hasher.update(graft::repo::OBJECT_FORMAT.as_bytes());
    hash_serialized(&mut hasher, index)?;
    let config = repo.config().map_err(repo_error)?;
    // Only settings that affect local classification belong in the persisted worktree proof.
    // Remote URLs, upstream configuration, and remote-tracking refs are refreshed separately.
    hash_serialized(&mut hasher, &config.files)?;
    hash_serialized(&mut hasher, &config.track)?;
    hash_serialized(&mut hasher, &config.worktree)?;
    hash_serialized(&mut hasher, &repo.head_target().map_err(repo_error)?)?;
    hash_serialized(&mut hasher, &repo.current_branch().map_err(repo_error)?)?;
    for name in ["HEAD", "MERGE_HEAD", "ORIG_HEAD"] {
        hash_optional_file(&mut hasher, repo.graft_dir(), &repo.graft_dir().join(name))?;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn ignore_source_fingerprint(repo: &Repository) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut matcher = repo.ignore_matcher().map_err(repo_error)?;
    hash_ignore_sources_in_directory(repo, &mut matcher, repo.worktree(), &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_ignore_sources_in_directory(
    repo: &Repository,
    matcher: &mut graft::repo::RepoIgnoreMatcher,
    directory: &Path,
    hasher: &mut blake3::Hasher,
) -> Result<()> {
    graft::repo::cancellation_checkpoint().map_err(repo_error)?;
    for name in [graft::repo::GIT_IGNORE_FILE, graft::repo::GRAFT_IGNORE_FILE] {
        hash_optional_file(hasher, repo.worktree(), &directory.join(name))?;
    }
    let entries = fs::read_dir(directory)
        .map_err(|error| repository_stale_io("scan ignore source directory", error))?;
    let mut directories = Vec::new();
    for entry in entries {
        graft::repo::cancellation_checkpoint().map_err(repo_error)?;
        let entry =
            entry.map_err(|error| repository_stale_io("scan ignore source entry", error))?;
        let path = entry.path();
        if repo.is_internal_worktree_path(&path) {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| repository_stale_io("inspect ignore source path", error))?;
        if !file_type.is_dir() {
            continue;
        }
        let key = repo.file_key(&path).map_err(repo_error)?;
        if !matcher.is_ignored(&key, true).map_err(repo_error)? {
            directories.push(path);
        }
    }
    directories.sort();
    for directory in directories {
        hash_ignore_sources_in_directory(repo, matcher, &directory, hasher)?;
    }
    Ok(())
}

fn hash_optional_file(hasher: &mut blake3::Hasher, base: &Path, path: &Path) -> Result<()> {
    graft::repo::cancellation_checkpoint().map_err(repo_error)?;
    let before = match fingerprint_path(path)? {
        Some(fingerprint) if fingerprint.is_file => fingerprint,
        Some(_) => {
            return Err(repository_stale_error(
                "metadata source changed path type while it was being read",
            ));
        }
        None => {
            hasher.update(b"absent\0");
            hasher.update(relative_hash_path(base, path).as_bytes());
            return Ok(());
        }
    };
    let bytes =
        fs::read(path).map_err(|error| repository_stale_io("read metadata source", error))?;
    let after = fingerprint_path(path)?.ok_or_else(|| {
        repository_stale_error("metadata source disappeared while it was being read")
    })?;
    if before != after {
        return Err(repository_stale_error(
            "metadata source changed while it was being read",
        ));
    }
    hasher.update(b"file\0");
    hasher.update(relative_hash_path(base, path).as_bytes());
    hasher.update(b"\0");
    hasher.update(&bytes);
    Ok(())
}

fn relative_hash_path(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn hash_serialized(hasher: &mut blake3::Hasher, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(status_encode_error)?;
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(&bytes);
    Ok(())
}

fn visible_untracked_fingerprints(
    repo: &Repository,
    files: &BTreeMap<String, CommitFileState>,
    artifacts: &BTreeMap<String, CommitArtifactState>,
) -> Result<BTreeMap<String, FileFingerprint>> {
    let mut visible = BTreeMap::new();
    let mut matcher = repo.ignore_matcher().map_err(repo_error)?;
    collect_visible_files(repo, &mut matcher, repo.worktree(), &mut visible)?;
    visible.retain(|key, _| {
        !files.contains_key(key) && !artifacts.contains_key(key) && !is_sqlite_sidecar_key(key)
    });
    Ok(visible)
}

fn collect_visible_files(
    repo: &Repository,
    matcher: &mut graft::repo::RepoIgnoreMatcher,
    directory: &Path,
    visible: &mut BTreeMap<String, FileFingerprint>,
) -> Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if is_worktree_race_io(&error) => {
            return Err(repository_stale_io("read worktree directory", error));
        }
        Err(error) => return Err(repository_command_error(error.into())),
    };
    for entry in entries {
        graft::repo::cancellation_checkpoint().map_err(repo_error)?;
        let entry = entry.map_err(|error| {
            if is_worktree_race_io(&error) {
                repository_stale_io("read worktree directory entry", error)
            } else {
                repository_command_error(error.into())
            }
        })?;
        let path = entry.path();
        if repo.is_internal_worktree_path(&path) {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| repository_stale_io("inspect worktree path type", error))?;
        let key = repo.file_key(&path).map_err(repo_error)?;
        if matcher
            .is_ignored(&key, file_type.is_dir())
            .map_err(repo_error)?
        {
            continue;
        }
        if file_type.is_dir() {
            collect_visible_files(repo, matcher, &path, visible)?;
        } else if file_type.is_file() {
            match fingerprint_path(&path)? {
                Some(fingerprint) if fingerprint.is_file => {
                    visible.insert(key, fingerprint);
                }
                Some(_) => {
                    return Err(repository_stale_error(
                        "worktree path changed type while it was being inspected",
                    ));
                }
                None => {
                    return Err(repository_stale_error(
                        "worktree path disappeared while it was being inspected",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn collect_ignored_files(
    repo: &Repository,
    matcher: &mut graft::repo::RepoIgnoreMatcher,
    directory: &Path,
    inherited_ignore: bool,
    ignored: &mut Vec<String>,
    paths_examined: &mut usize,
) -> Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if is_worktree_race_io(&error) => {
            return Err(repository_stale_io("read inventory directory", error));
        }
        Err(error) => return Err(repository_command_error(error.into())),
    };
    for entry in entries {
        graft::repo::cancellation_checkpoint().map_err(repo_error)?;
        let entry = entry.map_err(|error| repository_stale_io("read inventory entry", error))?;
        let path = entry.path();
        if repo.is_internal_worktree_path(&path) {
            continue;
        }
        *paths_examined = (*paths_examined).saturating_add(1);
        let file_type = entry
            .file_type()
            .map_err(|error| repository_stale_io("inspect inventory path type", error))?;
        let key = repo.file_key(&path).map_err(repo_error)?;
        let ignored_here = inherited_ignore
            || matcher
                .is_ignored(&key, file_type.is_dir())
                .map_err(repo_error)?;
        if file_type.is_dir() {
            collect_ignored_files(repo, matcher, &path, ignored_here, ignored, paths_examined)?;
        } else if file_type.is_file() && ignored_here {
            ignored.push(key);
        }
    }
    Ok(())
}

fn matching_fingerprint_count<K: Ord, V: PartialEq>(
    previous: &BTreeMap<K, V>,
    current: &BTreeMap<K, V>,
) -> usize {
    current
        .iter()
        .filter(|(key, value)| previous.get(key).is_some_and(|prior| prior == *value))
        .count()
}

fn status_changed(previous: Option<&RepoStatus>, current: &RepoStatus) -> Result<bool> {
    let Some(previous) = previous else {
        return Ok(true);
    };
    let previous = serde_json::to_vec(previous).map_err(status_encode_error)?;
    let current = serde_json::to_vec(current).map_err(status_encode_error)?;
    Ok(previous != current)
}

fn status_encode_error(error: serde_json::Error) -> SdkError {
    SdkError::new(
        SdkErrorCode::InvalidResponse,
        format!("could not encode repository status: {error}"),
    )
}

fn is_sqlite_sidecar_key(key: &str) -> bool {
    key.ends_with("-wal") || key.ends_with("-shm") || key.ends_with("-journal")
}

fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn safe_remote_projection(config: &RemoteConfig) -> (SafeRemoteKind, String) {
    match config {
        RemoteConfig::Memory => (SafeRemoteKind::Memory, "memory".to_string()),
        RemoteConfig::Fs { root } => (SafeRemoteKind::Fs, format!("fs://{root}")),
        RemoteConfig::S3Compatible { bucket, prefix, endpoint } => {
            let mut url = prefix.as_ref().map_or_else(
                || format!("s3://{bucket}"),
                |prefix| format!("s3://{bucket}/{prefix}"),
            );
            if let Some(endpoint) = endpoint {
                url.push_str("?endpoint=");
                url.push_str(endpoint);
            }
            (SafeRemoteKind::S3Compatible, url)
        }
        // `token_env` and any in-memory bearer credential are intentionally excluded.
        RemoteConfig::Http { url, .. } => (SafeRemoteKind::Http, url.clone()),
    }
}

fn normalize_requested_path(path: &Path) -> Result<String> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(invalid_argument(
            "diff paths must be non-empty repository-relative paths",
        ));
    }
    if path
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(invalid_argument(format!(
            "diff path `{}` is not a normalized repository-relative path",
            path.display()
        )));
    }
    let key = path
        .to_str()
        .ok_or_else(|| invalid_argument("diff paths must be valid UTF-8"))?
        .replace('\\', "/");
    graft::repo::validate_repo_path_identity(&key).map_err(repo_error)?;
    Ok(key)
}

fn normalize_batch_paths(paths: &[PathBuf]) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Err(invalid_argument("path collection must not be empty"));
    }
    if paths.len() > MAX_BATCH_MUTATION_PATHS {
        return Err(invalid_argument(format!(
            "path collection exceeds {MAX_BATCH_MUTATION_PATHS} paths"
        )));
    }
    let mut normalized = paths
        .iter()
        .map(|path| normalize_requested_path(path))
        .collect::<Result<Vec<_>>>()?;
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

fn normalize_ignore_query_paths(paths: &[PathBuf]) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Err(invalid_argument("ignore path collection must not be empty"));
    }
    if paths.len() > MAX_IGNORE_QUERY_PATHS {
        return Err(invalid_argument(format!(
            "ignore path collection exceeds {MAX_IGNORE_QUERY_PATHS} paths"
        )));
    }
    paths
        .iter()
        .map(|path| normalize_requested_path(path))
        .collect()
}

fn cached_path_is_tracked(
    files: &BTreeMap<String, CommitFileState>,
    artifacts: &BTreeMap<String, CommitArtifactState>,
    path: &str,
) -> bool {
    files.contains_key(path) || artifacts.contains_key(path)
}

fn cached_path_has_tracked_descendants(
    files: &BTreeMap<String, CommitFileState>,
    artifacts: &BTreeMap<String, CommitArtifactState>,
    path: &str,
) -> bool {
    let prefix = format!("{path}/");
    map_has_key_with_prefix(files, &prefix) || map_has_key_with_prefix(artifacts, &prefix)
}

fn map_has_key_with_prefix<V>(map: &BTreeMap<String, V>, prefix: &str) -> bool {
    map.range(prefix.to_string()..)
        .next()
        .is_some_and(|(candidate, _)| candidate.starts_with(prefix))
}

fn value_changed_path_count(value: &Value) -> usize {
    value
        .get("paths")
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

fn overlay_staged_renames(worktree: &mut Value, staged: &Value) {
    let renames = staged
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|path| path.get("change").and_then(Value::as_str) == Some("renamed"))
        .filter_map(|path| {
            Some((
                path.get("path")?.as_str()?.to_string(),
                path.get("previous_path")?.as_str()?.to_string(),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    if renames.is_empty() {
        return;
    }
    for field in ["paths", "files"] {
        let Some(entries) = worktree.get_mut(field).and_then(Value::as_array_mut) else {
            continue;
        };
        for entry in entries {
            let Some(object) = entry.as_object_mut() else {
                continue;
            };
            let Some(previous_path) = object
                .get("path")
                .and_then(Value::as_str)
                .and_then(|path| renames.get(path))
            else {
                continue;
            };
            object.insert("change".to_string(), Value::String("renamed".to_string()));
            object.insert(
                "previous_path".to_string(),
                Value::String(previous_path.clone()),
            );
        }
    }
}

fn value_row_diff_tables_scanned(value: &Value) -> usize {
    value
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|file| {
            file.get("telemetry")
                .and_then(|telemetry| telemetry.get("tables_scanned"))
                .and_then(Value::as_u64)
        })
        .map(|count| count as usize)
        .sum()
}

#[derive(Default)]
struct BoundedDiffTelemetryAggregate {
    response_scope: String,
    tables_scanned: usize,
    rows_scanned: usize,
    rows_returned: usize,
    truncated: bool,
}

fn bounded_diff_telemetry(results: &[PathDiffResult]) -> BoundedDiffTelemetryAggregate {
    let mut aggregate = BoundedDiffTelemetryAggregate::default();
    for file in results
        .iter()
        .filter_map(|entry| entry.diff.get("files").and_then(Value::as_array))
        .flatten()
    {
        let Some(telemetry) = file.get("telemetry") else {
            continue;
        };
        aggregate.tables_scanned += telemetry
            .get("tables_scanned")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        aggregate.rows_scanned += telemetry
            .get("rows_scanned")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        aggregate.rows_returned += telemetry
            .get("rows_returned")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        aggregate.truncated |= telemetry
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if aggregate.response_scope.is_empty() {
            aggregate.response_scope = telemetry
                .get("response_scope")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
        }
    }
    if aggregate.response_scope.is_empty() {
        aggregate.response_scope = "unavailable".to_string();
    }
    aggregate
}

fn validate_diff_path_page(paths: &[PathBuf], limit: usize) -> Result<()> {
    if paths.is_empty() {
        return Err(invalid_argument("diff paths must not be empty"));
    }
    if paths.len() > MAX_DIFF_PATH_REQUEST_SIZE {
        return Err(invalid_argument(format!(
            "diff paths request exceeds {MAX_DIFF_PATH_REQUEST_SIZE} paths"
        )));
    }
    if limit == 0 || limit > MAX_DIFF_PATH_PAGE_SIZE {
        return Err(invalid_argument(format!(
            "diff path limit must be between 1 and {MAX_DIFF_PATH_PAGE_SIZE}"
        )));
    }
    Ok(())
}

fn repo_error(error: graft::repo::RepoErr) -> SdkError {
    repository_command_error(error.into())
}

fn execute_json(
    service: &mut RepositoryCommandService,
    name: &str,
    argument: Option<&str>,
) -> Result<Value> {
    let command = RepositoryCommand::parse(name, argument).map_err(repository_command_error)?;
    let output = service
        .execute(command)
        .map_err(repository_command_error)?
        .ok_or_else(|| {
            SdkError::new(
                SdkErrorCode::InvalidResponse,
                format!("repository command `{name}` returned no JSON"),
            )
        })?;
    serde_json::from_str(&output).map_err(|error| {
        SdkError::new(
            SdkErrorCode::InvalidResponse,
            format!("repository command `{name}` returned invalid JSON: {error}"),
        )
    })
}

fn execute_json_command(
    service: &mut RepositoryCommandService,
    command: RepositoryCommand,
    operation: &str,
) -> Result<Value> {
    let output = service
        .execute(command)
        .map_err(repository_command_error)?
        .ok_or_else(|| {
            SdkError::new(
                SdkErrorCode::InvalidResponse,
                format!("repository command `{operation}` returned no JSON"),
            )
        })?;
    serde_json::from_str(&output).map_err(|error| {
        SdkError::new(
            SdkErrorCode::InvalidResponse,
            format!("repository command `{operation}` returned invalid JSON: {error}"),
        )
    })
}

fn repository_command_error(error: ErrCtx) -> SdkError {
    if matches!(
        &error,
        ErrCtx::Repo(graft::repo::RepoErr::Cancelled)
            | ErrCtx::Graft(graft::GraftErr::Logical(graft::LogicalErr::Cancelled))
    ) {
        return SdkError::new(SdkErrorCode::Cancelled, "operation cancelled");
    }
    let message = error.to_string();
    let code = sdk_error_code_for_error(&error, &message);
    SdkError::new(code, message)
}

fn sdk_error_code_for_error(error: &ErrCtx, message: &str) -> SdkErrorCode {
    #[cfg(target_arch = "wasm32")]
    {
        // Browser builds use graft::remote_wasm, which deliberately exposes a
        // smaller error surface because network remotes are not available in
        // the Playground. Keep the stable generic mapping here instead of
        // making the WASM SDK depend on native transport variants.
        let _ = error;
        return sdk_error_code_for_message(message);
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        use graft::{
            remote::{HttpTransportErrorKind, RemoteErr},
            repo::RepoErr,
        };

        match error {
            ErrCtx::Repo(RepoErr::Remote(RemoteErr::PublicationUnconfirmed { .. })) => {
                SdkErrorCode::RemotePublicationUnconfirmed
            }
            ErrCtx::Repo(RepoErr::Remote(RemoteErr::PublicationOutcomeUnknown { .. })) => {
                SdkErrorCode::RemotePublicationOutcomeUnknown
            }
            ErrCtx::Repo(RepoErr::Remote(remote_error))
                if remote_error.http_transport_kind() == Some(HttpTransportErrorKind::Timeout) =>
            {
                SdkErrorCode::RemoteTransportTimeout
            }
            ErrCtx::Repo(RepoErr::MergePlanStale { .. }) => SdkErrorCode::RepositoryStale,
            _ => sdk_error_code_for_message(message),
        }
    }
}

fn sdk_error_code_for_message(message: &str) -> SdkErrorCode {
    let lowercase = message.to_ascii_lowercase();
    if lowercase.contains("cancelled") || lowercase.contains("canceled") {
        SdkErrorCode::Cancelled
    } else if lowercase.contains("locked")
        || lowercase.contains("database lock")
        || lowercase.contains("already held")
    {
        SdkErrorCode::RepositoryBusy
    } else if lowercase.contains("no such file")
        || lowercase.contains("not a directory")
        || lowercase.contains("is a directory")
        || lowercase.contains("not a regular file")
        || lowercase.contains("os error 2")
        || lowercase.contains("os error 20")
        || lowercase.contains("os error 21")
    {
        SdkErrorCode::RepositoryStale
    } else {
        SdkErrorCode::RepositoryCommand
    }
}

fn is_worktree_race_io(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::NotADirectory
            | std::io::ErrorKind::IsADirectory
    )
}

fn repository_stale_io(context: &str, error: std::io::Error) -> SdkError {
    if is_worktree_race_io(&error) {
        repository_stale_error(format!("{context}: repository changed during operation"))
    } else {
        repository_command_error(error.into())
    }
}

fn repository_stale_error(message: impl Into<String>) -> SdkError {
    SdkError::new(SdkErrorCode::RepositoryStale, message)
}

fn credential_error(error: RemoteCredentialErr) -> SdkError {
    SdkError::new(SdkErrorCode::InvalidArgument, error.to_string())
}

fn lifecycle_from_raw(lifecycle: u8) -> SessionLifecycle {
    match lifecycle {
        LIFECYCLE_CLOSED => SessionLifecycle::Closed,
        LIFECYCLE_OPENING => SessionLifecycle::Opening,
        LIFECYCLE_OPEN => SessionLifecycle::Open,
        LIFECYCLE_CLOSING => SessionLifecycle::Closing,
        _ => unreachable!("invalid repository session lifecycle"),
    }
}

fn repository_session_target(target: &Path) -> PathBuf {
    if target
        .file_name()
        .is_some_and(|name| name == graft::repo::GRAFT_DIR)
    {
        return target.to_path_buf();
    }
    if target.is_dir() || !target.exists() {
        return target.join(graft::repo::GRAFT_DIR);
    }
    target.to_path_buf()
}

fn diff_argument(options: &DiffOptions) -> Result<Option<String>> {
    if options.staged && (options.root.is_some() || options.from.is_some() || options.to.is_some())
    {
        return Err(invalid_argument(
            "staged diff cannot be combined with revision targets",
        ));
    }
    if options.root.is_some() && (options.from.is_some() || options.to.is_some()) {
        return Err(invalid_argument(
            "root diff cannot be combined with from/to revisions",
        ));
    }
    if options.from.is_none() && options.to.is_some() {
        return Err(invalid_argument("diff `to` requires a `from` revision"));
    }
    if options.table.is_some() && !options.rows {
        return Err(invalid_argument("diff `table` requires row details"));
    }
    if options.table.is_some() && options.path.is_none() {
        return Err(invalid_argument("diff `table` requires one explicit path"));
    }

    let mut parts = Vec::new();
    if options.rows {
        parts.push("--rows".to_string());
    }
    if let Some(table) = &options.table {
        if table.is_empty() {
            return Err(invalid_argument("diff table must not be empty"));
        }
        if table.len() > 1_024 {
            return Err(invalid_argument("diff table exceeds 1,024 bytes"));
        }
        parts.push("--table".to_string());
        parts.push(quote_pragma_value(table)?);
    }
    if options.staged {
        parts.push("--staged".to_string());
    }
    if let Some(root) = &options.root {
        validate_revision(root)?;
        parts.push("--root".to_string());
        parts.push(root.clone());
    }
    if let Some(from) = &options.from {
        validate_revision(from)?;
        parts.push(from.clone());
        if let Some(to) = &options.to {
            validate_revision(to)?;
            parts.push(to.clone());
        }
    }
    if let Some(path) = &options.path {
        parts.push("--".to_string());
        parts.push(quote_pragma_path(path)?);
    }
    Ok((!parts.is_empty()).then(|| parts.join(" ")))
}

fn sqlite_diff_argument(options: &SqliteDiffPathsOptions, path: &Path) -> Result<String> {
    if options.staged && (options.root.is_some() || options.from.is_some() || options.to.is_some())
    {
        return Err(invalid_argument(
            "staged diff cannot be combined with root/from/to revisions",
        ));
    }
    if options.root.is_some() && (options.from.is_some() || options.to.is_some()) {
        return Err(invalid_argument(
            "root diff cannot be combined with from/to revisions",
        ));
    }
    if options.from.is_none() && options.to.is_some() {
        return Err(invalid_argument("diff `to` requires a `from` revision"));
    }
    let mut parts = Vec::new();
    match &options.response {
        SqliteDiffResponse::Summary => parts.push("--sqlite-summary".to_string()),
        SqliteDiffResponse::Rows { table, limit, after } => {
            if table.is_empty() || table.len() > 1_024 {
                return Err(invalid_argument(
                    "SQLite row diff table must contain between 1 and 1,024 bytes",
                ));
            }
            if *limit == 0 || *limit > 1_000 {
                return Err(invalid_argument(
                    "SQLite row diff limit must be between 1 and 1,000",
                ));
            }
            parts.extend([
                "--rows".to_string(),
                "--table".to_string(),
                quote_pragma_value(table)?,
                "--row-limit".to_string(),
                limit.to_string(),
            ]);
            if let Some(after) = after {
                if after.is_empty() || after.len() > 1_024 {
                    return Err(invalid_argument("SQLite row cursor is invalid"));
                }
                parts.push("--row-after".to_string());
                parts.push(quote_pragma_value(after)?);
            }
        }
    }
    if options.staged {
        parts.push("--staged".to_string());
    }
    if let Some(root) = &options.root {
        validate_revision(root)?;
        parts.push("--root".to_string());
        parts.push(root.clone());
    }
    if let Some(from) = &options.from {
        validate_revision(from)?;
        parts.push(from.clone());
        if let Some(to) = &options.to {
            validate_revision(to)?;
            parts.push(to.clone());
        }
    }
    parts.push("--".to_string());
    parts.push(quote_pragma_path(path)?);
    Ok(parts.join(" "))
}

fn quote_pragma_value(value: &str) -> Result<String> {
    if value.contains('\0') {
        return Err(invalid_argument("diff value must not contain NUL"));
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("\"{escaped}\""))
}

fn ensure_expected_head(repo: &Repository, expected_head: Option<&str>) -> Result<()> {
    let Some(expected_head) = expected_head else {
        return Ok(());
    };
    let actual = repo.head_target().map_err(repo_error)?;
    if actual.as_deref() != Some(expected_head) {
        return Err(repository_stale_error(format!(
            "repository HEAD changed: expected {expected_head}, found {}",
            actual.as_deref().unwrap_or("unborn")
        )));
    }
    Ok(())
}

fn merge_policy_result(policy: ServiceMergePolicy) -> Result<MergePolicyResult> {
    if policy.version != graft::repo::MERGE_POLICY_VERSION {
        return Err(SdkError::new(
            SdkErrorCode::InvalidResponse,
            format!("unsupported merge policy version {}", policy.version),
        ));
    }
    Ok(MergePolicyResult {
        policy: MergePolicyDocument {
            version: policy.version,
            config: policy.policy,
        },
        policy_token: policy.token,
        active_merge: policy.active_merge,
    })
}

fn merge_plan_result(
    plan: &MergePlan,
    policy_token: &str,
    policy_version: u32,
) -> Result<MergePlanResult> {
    let encoded = serde_json::to_vec(&(plan, policy_token, policy_version)).map_err(|error| {
        SdkError::new(
            SdkErrorCode::InvalidResponse,
            format!("failed to encode merge plan: {error}"),
        )
    })?;
    let plan_token = blake3::hash(&encoded).to_hex().to_string();
    let (kind, expected_head, merge_base, staged_paths, conflicted_paths) = match &plan.outcome {
        MergeOutcome::AlreadyUpToDate { head } => (
            MergePlanKind::UpToDate,
            Some(head.clone()),
            None,
            Vec::new(),
            Vec::new(),
        ),
        MergeOutcome::FastForward { from, .. } => (
            MergePlanKind::FastForward,
            from.clone(),
            None,
            Vec::new(),
            Vec::new(),
        ),
        MergeOutcome::Merged { head, merge_base, staged, conflicted, .. } => (
            MergePlanKind::ThreeWay,
            Some(head.clone()),
            merge_base.clone(),
            staged.clone(),
            conflicted.clone(),
        ),
    };
    Ok(MergePlanResult {
        kind,
        expected_head,
        target: plan.target.clone(),
        merge_base,
        staged_paths,
        conflicted_paths,
        plan_token,
        policy_token: policy_token.to_string(),
        policy_version,
    })
}

fn merge_status_from_incremental(
    service: &mut RepositoryCommandService,
    incremental: &IncrementalStatusResult,
) -> Result<MergeStatus> {
    let Some(merge_head) = incremental.status.merge_head.clone() else {
        return Ok(MergeStatus::None);
    };
    let orig_head = incremental.status.orig_head.clone().ok_or_else(|| {
        SdkError::new(
            SdkErrorCode::InvalidResponse,
            "repository has MERGE_HEAD without ORIG_HEAD",
        )
    })?;
    let repo = service.repository().map_err(repository_command_error)?;
    let policy = service.merge_policy().map_err(repository_command_error)?;
    let merge_base = repo
        .merge_base_between(&orig_head, &merge_head)
        .map_err(repo_error)?;
    let index = repo.read_index().map_err(repo_error)?;
    let state_token = durable_merge_state_token(&repo, &incremental.status, &index)?;
    Ok(MergeStatus::Merging {
        orig_head,
        merge_head,
        merge_base,
        staged_count: incremental.status.counts.staged,
        unmerged_count: incremental.status.counts.conflicted,
        state_token,
        policy_token: policy.token,
        policy_version: policy.version,
    })
}

fn merge_worktree_paths_from_output(output: &Value) -> Vec<String> {
    let mut paths = BTreeSet::new();
    let Some(object) = output.as_object() else {
        return Vec::new();
    };

    match object.get("materialized") {
        Some(Value::Bool(true)) => {
            if let Some(path) = object.get("path").and_then(Value::as_str) {
                paths.insert(path.to_string());
            }
        }
        Some(Value::Array(actions)) => {
            for action in actions {
                if let Some(path) = action.get("path").and_then(Value::as_str) {
                    paths.insert(path.to_string());
                }
            }
        }
        _ => {}
    }

    if let Some(actions) = object.get("paths").and_then(Value::as_array) {
        for action in actions {
            let materializes = action
                .get("action")
                .and_then(Value::as_str)
                .is_some_and(|action| matches!(action, "checked_out" | "materialized" | "removed"));
            if materializes && let Some(path) = action.get("path").and_then(Value::as_str) {
                paths.insert(path.to_string());
            }
        }
    }

    paths.into_iter().collect()
}

fn active_merge_heads(
    repo: &Repository,
    incremental: &IncrementalStatusResult,
) -> Result<(String, String, Option<String>)> {
    let merge_head =
        incremental.status.merge_head.clone().ok_or_else(|| {
            SdkError::new(SdkErrorCode::RepositoryCommand, "no merge in progress")
        })?;
    let orig_head = incremental.status.orig_head.clone().ok_or_else(|| {
        SdkError::new(
            SdkErrorCode::InvalidResponse,
            "repository has MERGE_HEAD without ORIG_HEAD",
        )
    })?;
    let merge_base = repo
        .merge_base_between(&orig_head, &merge_head)
        .map_err(repo_error)?;
    Ok((orig_head, merge_head, merge_base))
}

fn require_merge_state_token(
    service: &mut RepositoryCommandService,
    status_cache: &mut IncrementalStatusCache,
    expected_state_token: &str,
) -> Result<IncrementalStatusResult> {
    if expected_state_token.trim().is_empty() {
        return Err(invalid_argument("merge state token must not be empty"));
    }
    let incremental = refresh_incremental_status(service, status_cache)?;
    if incremental.status.merge_head.is_none() {
        return Err(SdkError::new(
            SdkErrorCode::RepositoryCommand,
            "no merge in progress",
        ));
    }
    let repo = service.repository().map_err(repository_command_error)?;
    let index = repo.read_index().map_err(repo_error)?;
    let actual_state_token = durable_merge_state_token(&repo, &incremental.status, &index)?;
    if actual_state_token != expected_state_token {
        return Err(repository_stale_error(
            "merge state changed; refresh the merge before retrying",
        ));
    }
    Ok(incremental)
}

fn durable_merge_state_token(
    repo: &Repository,
    status: &RepoStatus,
    index: &Index,
) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"graft-merge-state-v1\0");
    hash_serialized(&mut hasher, status)?;
    hash_serialized(&mut hasher, index)?;
    hash_optional_file(
        &mut hasher,
        repo.graft_dir(),
        &repo.graft_dir().join("merge-resolution-session.json"),
    )?;
    Ok(format!("graft-merge-v1:{}", hasher.finalize().to_hex()))
}

fn validate_semantic_provider_name(provider: &str) -> Result<()> {
    if provider.is_empty() || provider.len() > MAX_SEMANTIC_PROVIDER_NAME_BYTES {
        return Err(invalid_argument(format!(
            "semantic merge provider must contain between 1 and {MAX_SEMANTIC_PROVIDER_NAME_BYTES} bytes"
        )));
    }
    if !provider
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
    {
        return Err(invalid_argument(
            "semantic merge provider may contain only ASCII letters, digits, '.', '_', ':', and '-'",
        ));
    }
    Ok(())
}

fn validate_semantic_managed_tables(tables: &[String]) -> Result<Vec<String>> {
    if tables.is_empty() || tables.len() > MAX_SEMANTIC_MANAGED_TABLES {
        return Err(invalid_argument(format!(
            "semantic merge managed tables must contain between 1 and {MAX_SEMANTIC_MANAGED_TABLES} entries"
        )));
    }
    let mut normalized = BTreeSet::new();
    for table in tables {
        if table.is_empty() || table.len() > 1_024 || table.contains('\0') {
            return Err(invalid_argument(
                "semantic merge managed table names must contain between 1 and 1,024 bytes and no NUL",
            ));
        }
        if !normalized.insert(table.clone()) {
            return Err(invalid_argument(
                "semantic merge managed table names must be unique",
            ));
        }
    }
    Ok(normalized.into_iter().collect())
}

#[allow(clippy::too_many_arguments)]
fn semantic_merge_provider_token(
    provider: &str,
    path: &str,
    state_token: &str,
    policy_token: &str,
    policy_version: u32,
    orig_head: &str,
    merge_head: &str,
    merge_base: Option<&str>,
    managed_tables: &[String],
) -> Result<String> {
    let encoded = serde_json::to_vec(&(
        SEMANTIC_MERGE_WORKSPACE_VERSION,
        provider,
        path,
        state_token,
        policy_token,
        policy_version,
        orig_head,
        merge_head,
        merge_base,
        managed_tables,
    ))
    .map_err(status_encode_error)?;
    Ok(format!(
        "graft-semantic-v1:{}",
        blake3::hash(&encoded).to_hex()
    ))
}

fn semantic_merge_workspace_path(repo: &Repository, provider_token: &str) -> Result<PathBuf> {
    let Some(digest) = provider_token.strip_prefix("graft-semantic-v1:") else {
        return Err(invalid_argument("invalid semantic merge provider token"));
    };
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid_argument("invalid semantic merge provider token"));
    }
    Ok(repo
        .graft_dir()
        .join(SEMANTIC_MERGE_WORKSPACE_DIRECTORY)
        .join(digest))
}

fn export_semantic_merge_input(
    service: &mut RepositoryCommandService,
    path: &str,
    version: MergeSqliteVersion,
    revision: Option<&str>,
    output: &Path,
) -> Result<SemanticMergeInput> {
    let Some(revision) = revision else {
        return Ok(SemanticMergeInput {
            version,
            revision: None,
            file_path: None,
            size: None,
        });
    };
    let repo = service.repository().map_err(repository_command_error)?;
    let physical_path = repo.worktree().join(path);
    let has_sqlite = repo
        .file_from_revision(revision, &physical_path)
        .map_err(repo_error)?
        .is_some();
    if !has_sqlite {
        if repo
            .artifact_from_revision(revision, &physical_path)
            .map_err(repo_error)?
            .is_some()
        {
            return Err(invalid_argument(format!(
                "merge {version:?} path `{path}` is not a SQLite database"
            )));
        }
        return Ok(SemanticMergeInput {
            version,
            revision: Some(revision.to_string()),
            file_path: None,
            size: None,
        });
    }
    service
        .export_revision_sqlite_path(revision, &physical_path, output)
        .map_err(repository_command_error)?;
    let metadata =
        fs::metadata(output).map_err(|error| repository_command_error(ErrCtx::IoErr(error)))?;
    let mut permissions = metadata.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(output, permissions)
        .map_err(|error| repository_command_error(ErrCtx::IoErr(error)))?;
    Ok(SemanticMergeInput {
        version,
        revision: Some(revision.to_string()),
        file_path: Some(output.to_string_lossy().into_owned()),
        size: Some(metadata.len()),
    })
}

fn read_semantic_merge_manifest(path: &Path) -> Result<SemanticMergeWorkspaceManifest> {
    let bytes = fs::read(path).map_err(|error| repository_command_error(ErrCtx::IoErr(error)))?;
    if bytes.len() > MAX_SEMANTIC_MERGE_RECORD_BYTES {
        return Err(SdkError::new(
            SdkErrorCode::InvalidResponse,
            "semantic merge manifest exceeds the supported size",
        ));
    }
    let manifest =
        serde_json::from_slice::<SemanticMergeWorkspaceManifest>(&bytes).map_err(|error| {
            SdkError::new(
                SdkErrorCode::InvalidResponse,
                format!("invalid semantic merge manifest: {error}"),
            )
        })?;
    if manifest.version != SEMANTIC_MERGE_WORKSPACE_VERSION {
        return Err(SdkError::new(
            SdkErrorCode::InvalidResponse,
            format!(
                "unsupported semantic merge workspace version {}",
                manifest.version
            ),
        ));
    }
    Ok(manifest)
}

fn write_semantic_merge_manifest(
    path: &Path,
    manifest: &SemanticMergeWorkspaceManifest,
) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest).map_err(status_encode_error)?;
    if bytes.len() > MAX_SEMANTIC_MERGE_RECORD_BYTES {
        return Err(invalid_argument(
            "semantic merge manifest exceeds the supported size",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid_argument("semantic merge manifest path has no parent directory"))?;
    fs::create_dir_all(parent).map_err(|error| repository_command_error(ErrCtx::IoErr(error)))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".manifest-{}-{nonce}.tmp", std::process::id()));
    let backup = parent.join(format!(".manifest-{}-{nonce}.backup", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| repository_command_error(ErrCtx::IoErr(error)))?;
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(repository_command_error(ErrCtx::IoErr(error)));
    }
    drop(file);
    let had_original = path.is_file();
    if had_original && let Err(error) = fs::rename(path, &backup) {
        let _ = fs::remove_file(&temporary);
        return Err(repository_command_error(ErrCtx::IoErr(error)));
    }
    if let Err(error) = fs::rename(&temporary, path) {
        if had_original {
            let _ = fs::rename(&backup, path);
        }
        let _ = fs::remove_file(&temporary);
        return Err(repository_command_error(ErrCtx::IoErr(error)));
    }
    if had_original {
        let _ = fs::remove_file(backup);
    }
    let _ = fs::File::open(parent).and_then(|directory| directory.sync_all());
    Ok(())
}

fn validate_semantic_merge_manifest(
    manifest: &SemanticMergeWorkspaceManifest,
    provider_token: &str,
    provider: &str,
    path: &str,
    state_token: &str,
    managed_tables: &[String],
) -> Result<()> {
    validate_semantic_merge_record_state(manifest, provider_token, state_token)?;
    if manifest.workspace.provider_token != provider_token
        || manifest.workspace.provider != provider
        || manifest.workspace.path != path
        || manifest.workspace.state_token != state_token
        || manifest.workspace.managed_tables != managed_tables
    {
        return Err(repository_stale_error(
            "semantic merge workspace does not match the active merge state",
        ));
    }
    Ok(())
}

fn validate_semantic_merge_workspace_paths(
    manifest: &SemanticMergeWorkspaceManifest,
    workspace_path: &Path,
) -> Result<()> {
    if Path::new(&manifest.workspace.workspace_path) != workspace_path
        || Path::new(&manifest.workspace.result_path) != workspace_path.join("result.sqlite")
        || !Path::new(&manifest.workspace.result_path).is_file()
    {
        return Err(SdkError::new(
            SdkErrorCode::InvalidResponse,
            "semantic merge manifest contains invalid workspace paths",
        ));
    }
    if manifest.workspace.inputs.len() != 3 {
        return Err(SdkError::new(
            SdkErrorCode::InvalidResponse,
            "semantic merge manifest must contain exactly three input descriptors",
        ));
    }
    let mut seen = [false; 3];
    for input in &manifest.workspace.inputs {
        let (index, expected_name) = match input.version {
            MergeSqliteVersion::Base => (0, "base.sqlite"),
            MergeSqliteVersion::Ours => (1, "ours.sqlite"),
            MergeSqliteVersion::Theirs => (2, "theirs.sqlite"),
        };
        if seen[index] {
            return Err(SdkError::new(
                SdkErrorCode::InvalidResponse,
                "semantic merge manifest contains duplicate input versions",
            ));
        }
        seen[index] = true;
        if let Some(file_path) = &input.file_path
            && (Path::new(file_path) != workspace_path.join(expected_name)
                || !Path::new(file_path).is_file())
        {
            return Err(SdkError::new(
                SdkErrorCode::InvalidResponse,
                "semantic merge manifest contains an invalid input path",
            ));
        }
    }
    Ok(())
}

fn validate_semantic_merge_record_state(
    manifest: &SemanticMergeWorkspaceManifest,
    provider_token: &str,
    state_token: &str,
) -> Result<()> {
    let expected_provider_token = semantic_merge_provider_token(
        &manifest.workspace.provider,
        &manifest.workspace.path,
        &manifest.workspace.state_token,
        &manifest.workspace.policy_token,
        manifest.workspace.policy_version,
        &manifest.workspace.orig_head,
        &manifest.workspace.merge_head,
        manifest.workspace.merge_base.as_deref(),
        &manifest.workspace.managed_tables,
    )?;
    if manifest.workspace.provider_token != provider_token
        || expected_provider_token != provider_token
        || manifest.workspace.state_token != state_token
    {
        return Err(repository_stale_error(
            "semantic merge workspace is stale; prepare it again",
        ));
    }
    Ok(())
}

fn ensure_semantic_merge_record_bounded(record: &SemanticMergeProviderRecord) -> Result<()> {
    let bytes = serde_json::to_vec(record).map_err(status_encode_error)?;
    if bytes.len() > MAX_SEMANTIC_MERGE_RECORD_BYTES {
        return Err(invalid_argument(
            "semantic merge provider record exceeds the supported size",
        ));
    }
    Ok(())
}

fn validate_page_limit(limit: usize, maximum: usize, label: &str) -> Result<()> {
    if limit == 0 || limit > maximum {
        return Err(invalid_argument(format!(
            "{label} limit must be between 1 and {maximum}"
        )));
    }
    Ok(())
}

fn page_start_for_cursor<T>(
    items: &[T],
    after: Option<&str>,
    key: impl Fn(&T) -> String,
    label: &str,
) -> Result<usize> {
    let Some(after) = after else {
        return Ok(0);
    };
    items
        .iter()
        .position(|item| key(item) == after)
        .map(|index| index + 1)
        .ok_or_else(|| invalid_argument(format!("unknown {label} cursor")))
}

fn merge_paths_for_index(index: &Index) -> Vec<MergePath> {
    let mut entries = BTreeMap::<String, Vec<&IndexEntry>>::new();
    for entry in &index.entries {
        entries.entry(entry.path.clone()).or_default().push(entry);
    }
    entries
        .into_iter()
        .map(|(path, entries)| {
            let state = if entries
                .iter()
                .any(|entry| entry.stage != IndexStage::Normal)
            {
                MergePathState::Unmerged
            } else {
                MergePathState::Resolved
            };
            let (kind, storage) = merge_path_descriptor(&entries);
            MergePath {
                path,
                state,
                kind,
                storage,
                has_base: entries.iter().any(|entry| entry.stage == IndexStage::Base),
                has_ours: entries.iter().any(|entry| entry.stage == IndexStage::Ours),
                has_theirs: entries
                    .iter()
                    .any(|entry| entry.stage == IndexStage::Theirs),
            }
        })
        .collect()
}

fn merge_path_descriptor(entries: &[&IndexEntry]) -> (RepoTrackedPathKind, RepoPathStorage) {
    if entries.iter().any(|entry| entry.file.is_some()) {
        return (
            RepoTrackedPathKind::SqliteDatabase,
            RepoPathStorage::SqliteSnapshot,
        );
    }
    if let Some(artifact) = entries.iter().find_map(|entry| entry.artifact.as_ref()) {
        return (
            artifact.kind(),
            if artifact.is_large() {
                RepoPathStorage::External
            } else {
                RepoPathStorage::Inline
            },
        );
    }
    (RepoTrackedPathKind::BinaryFile, RepoPathStorage::Inline)
}

fn conflict_id(conflict: &Value) -> &str {
    conflict.get("id").and_then(Value::as_str).unwrap_or("")
}

fn project_merge_content(
    version: &str,
    revision: Option<String>,
    content: RepoPathContent,
    state_token: String,
) -> MergeContent {
    let content_state = match content.content {
        RepoPathContentState::Absent => MergeContentState::Absent,
        RepoPathContentState::Utf8 { content, size, .. } => {
            MergeContentState::Utf8 { content, size }
        }
        RepoPathContentState::TooLarge { size, .. } => MergeContentState::TooLarge { size },
        RepoPathContentState::MissingPayload { size, .. } => {
            MergeContentState::MissingPayload { size }
        }
        RepoPathContentState::InvalidUtf8 { size, .. } => MergeContentState::InvalidUtf8 { size },
    };
    MergeContent {
        version: version.to_string(),
        revision,
        path: content.path,
        kind: content.kind,
        storage: content.storage,
        content: content_state,
        state_token,
    }
}

fn read_worktree_merge_content(
    repo: &Repository,
    path: &str,
    max_bytes: u64,
    state_token: String,
) -> Result<MergeContent> {
    let index = repo.read_index().map_err(repo_error)?;
    let path_entries = index
        .entries
        .iter()
        .filter(|entry| entry.path == path)
        .collect::<Vec<_>>();
    let descriptor = (!path_entries.is_empty()).then(|| merge_path_descriptor(&path_entries));
    let physical_path = repo.worktree().join(path);
    let metadata = match fs::metadata(&physical_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MergeContent {
                version: "result".to_string(),
                revision: None,
                path: path.to_string(),
                kind: descriptor.map(|value| value.0),
                storage: descriptor.map(|value| value.1),
                content: MergeContentState::Absent,
                state_token,
            });
        }
        Err(error) => return Err(repository_command_error(ErrCtx::IoErr(error))),
    };
    let size = metadata.len();
    let content = if size > max_bytes {
        MergeContentState::TooLarge { size }
    } else {
        let bytes = fs::read(&physical_path)
            .map_err(|error| repository_command_error(ErrCtx::IoErr(error)))?;
        match String::from_utf8(bytes) {
            Ok(content) => MergeContentState::Utf8 { content, size },
            Err(_) => MergeContentState::InvalidUtf8 { size },
        }
    };
    Ok(MergeContent {
        version: "result".to_string(),
        revision: None,
        path: path.to_string(),
        kind: descriptor.map(|value| value.0),
        storage: descriptor.map(|value| value.1),
        content,
        state_token,
    })
}

fn ensure_text_merge_conflict(repo: &Repository, path: &str) -> Result<()> {
    let index = repo.read_index().map_err(repo_error)?;
    let entries = index
        .entries
        .iter()
        .filter(|entry| entry.path == path)
        .collect::<Vec<_>>();
    if !entries
        .iter()
        .any(|entry| entry.stage != IndexStage::Normal)
    {
        return Err(repository_command_error(ErrCtx::Repo(
            graft::repo::RepoErr::PathNotConflicted(path.to_string()),
        )));
    }
    let artifacts = entries
        .iter()
        .filter_map(|entry| entry.artifact.as_ref())
        .collect::<Vec<_>>();
    if artifacts.is_empty()
        || entries.iter().any(|entry| entry.file.is_some())
        || artifacts
            .iter()
            .any(|artifact| artifact.kind() != RepoTrackedPathKind::TextFile)
    {
        return Err(repository_command_error(ErrCtx::Repo(
            graft::repo::RepoErr::PathNotTextArtifact(path.to_string()),
        )));
    }
    Ok(())
}

fn write_merge_text_result(repo: &Repository, path: &Path, bytes: &[u8]) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Err(invalid_argument(
            "edited merge path has no parent directory",
        ));
    };
    fs::create_dir_all(parent).map_err(|error| repository_command_error(ErrCtx::IoErr(error)))?;
    repo.file_key(path).map_err(repo_error)?;
    let temp_directory = repo.graft_dir().join("tmp");
    fs::create_dir_all(&temp_directory)
        .map_err(|error| repository_command_error(ErrCtx::IoErr(error)))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_path = temp_directory.join(format!(
        "sdk-merge-result-{}-{nonce}.tmp",
        std::process::id()
    ));
    let backup_path = temp_directory.join(format!(
        "sdk-merge-result-{}-{nonce}.backup",
        std::process::id()
    ));
    let mut temp = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|error| repository_command_error(ErrCtx::IoErr(error)))?;
    if let Err(error) = temp.write_all(bytes).and_then(|()| temp.sync_all()) {
        let _ = fs::remove_file(&temp_path);
        return Err(repository_command_error(ErrCtx::IoErr(error)));
    }
    drop(temp);
    let had_original = path.exists();
    if had_original {
        fs::rename(path, &backup_path)
            .map_err(|error| repository_command_error(ErrCtx::IoErr(error)))?;
    }
    if let Err(error) = fs::rename(&temp_path, path) {
        if had_original {
            let _ = fs::rename(&backup_path, path);
        }
        let _ = fs::remove_file(&temp_path);
        return Err(repository_command_error(ErrCtx::IoErr(error)));
    }
    if had_original {
        let _ = fs::remove_file(backup_path);
    }
    Ok(())
}

fn remote_branch_argument(remote: Option<&str>, branch: Option<&str>) -> Result<Option<String>> {
    if let Some(remote) = remote {
        validate_remote_name(remote)?;
    }
    if let Some(branch) = branch {
        validate_branch_name(branch)?;
    }
    Ok(match (remote, branch) {
        (None, None) => None,
        (Some(remote), None) => Some(remote.to_string()),
        (Some(remote), Some(branch)) => Some(format!("{remote} {branch}")),
        (None, Some(branch)) => Some(format!("origin {branch}")),
    })
}

fn remote_url<'a>(remotes: &'a Value, name: &str) -> Result<Option<&'a str>> {
    let entries = remotes
        .get("remotes")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            SdkError::new(
                SdkErrorCode::InvalidResponse,
                "remote list response does not contain `remotes`",
            )
        })?;
    Ok(entries.iter().find_map(|entry| {
        (entry.get("name").and_then(Value::as_str) == Some(name))
            .then(|| entry.get("url").and_then(Value::as_str))
            .flatten()
    }))
}

fn validate_sdk_remote_url(url: &str) -> Result<()> {
    if url.trim() != url
        || url.is_empty()
        || url
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(invalid_argument(
            "remote URL must be a non-empty URI without whitespace",
        ));
    }
    let http = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("graft+https://"))
        .or_else(|| url.strip_prefix("graft+http://"));
    if let Some(location) = http {
        let authority = location.split('/').next().unwrap_or_default();
        if authority.contains('@') || location.contains(['?', '#']) {
            return Err(invalid_argument(
                "SDK HTTP remote URLs must not contain credentials, query parameters, or fragments",
            ));
        }
    }
    Ok(())
}

fn validate_remote_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || name
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        || name.contains(['/', '\\'])
    {
        return Err(invalid_argument("invalid repository remote name"));
    }
    Ok(())
}

fn validate_branch_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || name
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(invalid_argument("invalid repository branch name"));
    }
    Ok(())
}

fn validate_revision(revision: &str) -> Result<()> {
    if revision.is_empty()
        || revision
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        || revision.starts_with('-')
    {
        return Err(invalid_argument("invalid repository revision"));
    }
    Ok(())
}

fn quote_pragma_path(path: &Path) -> Result<String> {
    let raw = path
        .to_str()
        .ok_or_else(|| invalid_argument("repository path is not valid UTF-8"))?;
    let escaped = raw.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("\"{escaped}\""))
}

fn invalid_argument(message: impl Into<String>) -> SdkError {
    SdkError::new(SdkErrorCode::InvalidArgument, message)
}

fn session_closed_error() -> SdkError {
    SdkError::new(SdkErrorCode::SessionClosed, "repository session is closed")
}

fn session_opening_error() -> SdkError {
    SdkError::new(
        SdkErrorCode::SessionOpening,
        "repository session is opening",
    )
}

fn session_closing_error() -> SdkError {
    SdkError::new(
        SdkErrorCode::SessionClosing,
        "repository session is closing",
    )
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Barrier, mpsc},
        thread,
        time::Duration,
    };

    use rusqlite::Connection;
    use serde_json::json;

    use super::*;

    #[test]
    fn operation_materialization_contract_is_explicit() {
        assert!(RepositoryOperation::Restore.materializes_worktree());
        assert!(RepositoryOperation::RestorePaths.materializes_worktree());
        assert!(RepositoryOperation::Pull.materializes_worktree());
        assert!(RepositoryOperation::Clone.materializes_worktree());
        assert!(RepositoryOperation::ApplyMerge.materializes_worktree());
        assert!(RepositoryOperation::SetMergePathResult.materializes_worktree());
        assert!(RepositoryOperation::UnresolveMergePath.materializes_worktree());
        assert!(RepositoryOperation::ResolveMergeRow.materializes_worktree());
        assert!(RepositoryOperation::ResolveMergeCell.materializes_worktree());
        assert!(RepositoryOperation::ResolveMergeTable.materializes_worktree());
        assert!(RepositoryOperation::WriteAndStageTextResult.materializes_worktree());
        assert!(RepositoryOperation::ContinueMerge.materializes_worktree());
        assert!(RepositoryOperation::AbortMerge.materializes_worktree());
        assert!(!RepositoryOperation::Init.materializes_worktree());
        assert!(!RepositoryOperation::Status.materializes_worktree());
        assert!(!RepositoryOperation::StatusIncremental.materializes_worktree());
        assert!(!RepositoryOperation::Diff.materializes_worktree());
        assert!(!RepositoryOperation::DiffPaths.materializes_worktree());
        assert!(!RepositoryOperation::ReadPathContent.materializes_worktree());
        assert!(!RepositoryOperation::AddAll.materializes_worktree());
        assert!(!RepositoryOperation::StagePaths.materializes_worktree());
        assert!(!RepositoryOperation::RecordPathMove.materializes_worktree());
        assert!(!RepositoryOperation::UntrackPaths.materializes_worktree());
        assert!(!RepositoryOperation::Commit.materializes_worktree());
        assert!(!RepositoryOperation::History.materializes_worktree());
        assert!(!RepositoryOperation::HistorySummaries.materializes_worktree());
        assert!(!RepositoryOperation::CommitDetails.materializes_worktree());
        assert!(!RepositoryOperation::CommitChangedPaths.materializes_worktree());
        assert!(!RepositoryOperation::IsIgnoredPath.materializes_worktree());
        assert!(!RepositoryOperation::IsIgnoredPaths.materializes_worktree());
        assert!(!RepositoryOperation::Inventory.materializes_worktree());
        assert!(!RepositoryOperation::RepositoryMetadata.materializes_worktree());
        assert!(!RepositoryOperation::ListRemotes.materializes_worktree());
        assert!(!RepositoryOperation::RemoteConfigure.materializes_worktree());
        assert!(!RepositoryOperation::Push.materializes_worktree());
        assert!(!RepositoryOperation::Fetch.materializes_worktree());
        assert!(!RepositoryOperation::PlanMerge.materializes_worktree());
        assert!(!RepositoryOperation::GetMergeStatus.materializes_worktree());
        assert!(!RepositoryOperation::ListMergePaths.materializes_worktree());
        assert!(!RepositoryOperation::GetMergePolicy.materializes_worktree());
        assert!(!RepositoryOperation::ValidateMergePolicy.materializes_worktree());
        assert!(!RepositoryOperation::SetMergePolicy.materializes_worktree());
        assert!(!RepositoryOperation::StageMergeSqliteResult.materializes_worktree());
        assert!(!RepositoryOperation::ListMergeConflicts.materializes_worktree());
        assert!(!RepositoryOperation::ReadMergeVersion.materializes_worktree());
        assert!(!RepositoryOperation::DiffMergeSqlite.materializes_worktree());
    }

    #[test]
    fn merge_plan_applies_to_an_unborn_branch() {
        let directory = tempfile::tempdir().unwrap();
        let note = directory.path().join("note.txt");
        let repo = Repository::init(directory.path()).unwrap();
        fs::write(&note, "target\n").unwrap();
        repo.stage_artifact_path(&note).unwrap();
        let target = repo.commit_staged("target").unwrap();
        repo.branch_create_unborn("empty").unwrap();
        repo.switch_branch("empty").unwrap();
        fs::remove_file(&note).unwrap();
        drop(repo);

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        let plan = session
            .plan_merge(&PlanMergeOptions {
                revision: target.id.clone(),
                expected_head: None,
            })
            .unwrap();
        assert_eq!(plan.kind, MergePlanKind::FastForward);
        assert_eq!(plan.expected_head, None);

        let applied = session
            .apply_merge(&ApplyMergeOptions {
                revision: target.id.clone(),
                expected_head: None,
                plan_token: plan.plan_token,
            })
            .unwrap();
        assert_eq!(applied.merge, MergeStatus::None);
        assert_eq!(fs::read_to_string(&note).unwrap(), "target\n");
        assert_eq!(
            session.repository_metadata().unwrap().current_head,
            Some(target.id)
        );
    }

    #[test]
    fn git_like_text_merge_survives_reopen_and_stages_edited_result() {
        let directory = tempfile::tempdir().unwrap();
        let note = directory.path().join("note.txt");
        let repo = Repository::init(directory.path()).unwrap();

        fs::write(&note, "base\n").unwrap();
        repo.stage_artifact_path(&note).unwrap();
        let base = repo.commit_staged("base").unwrap();
        repo.switch_new_branch("hosted", None).unwrap();
        fs::write(&note, "hosted\n").unwrap();
        repo.stage_artifact_path(&note).unwrap();
        let theirs = repo.commit_staged("hosted edit").unwrap();
        repo.switch_branch("main").unwrap();
        fs::write(&note, "local\n").unwrap();
        repo.stage_artifact_path(&note).unwrap();
        let ours = repo.commit_staged("local edit").unwrap();
        drop(repo);

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        let plan = session
            .plan_merge(&PlanMergeOptions {
                revision: theirs.id.clone(),
                expected_head: Some(ours.id.clone()),
            })
            .unwrap();
        assert_eq!(plan.kind, MergePlanKind::ThreeWay);
        assert_eq!(plan.merge_base.as_deref(), Some(base.id.as_str()));
        assert_eq!(plan.conflicted_paths, vec!["note.txt"]);

        let applied = session
            .apply_merge(&ApplyMergeOptions {
                revision: theirs.id.clone(),
                expected_head: Some(ours.id.clone()),
                plan_token: plan.plan_token,
            })
            .unwrap();
        let MergeStatus::Merging {
            orig_head,
            merge_head,
            unmerged_count,
            state_token,
            ..
        } = applied.merge
        else {
            panic!("expected an active merge");
        };
        assert_eq!(orig_head, ours.id);
        assert_eq!(merge_head, theirs.id);
        assert_eq!(unmerged_count, 1);

        let paths = session
            .list_merge_paths(&ListMergePathsOptions {
                filter: MergePathFilter::All,
                limit: 10,
                after: None,
                expected_state_token: state_token.clone(),
            })
            .unwrap();
        assert_eq!(paths.items.len(), 1);
        assert_eq!(paths.items[0].path, "note.txt");
        assert_eq!(paths.items[0].state, MergePathState::Unmerged);
        assert!(paths.items[0].has_base);
        assert!(paths.items[0].has_ours);
        assert!(paths.items[0].has_theirs);

        for (version, expected) in [
            (MergeVersion::Base, "base\n"),
            (MergeVersion::Ours, "local\n"),
            (MergeVersion::Theirs, "hosted\n"),
        ] {
            let content = session
                .read_merge_version(&ReadMergeVersionOptions {
                    path: PathBuf::from("note.txt"),
                    version,
                    max_bytes: 1024,
                    expected_state_token: state_token.clone(),
                })
                .unwrap();
            assert!(matches!(
                content.content,
                MergeContentState::Utf8 { ref content, .. } if content == expected
            ));
        }

        session.close().unwrap();
        session.open().unwrap();
        let reopened = session.get_merge_status().unwrap();
        let MergeStatus::Merging { state_token: reopened_token, .. } = reopened else {
            panic!("expected merge state after reopen");
        };
        assert_eq!(reopened_token, state_token);

        let stale = session
            .write_and_stage_text_result(&WriteAndStageTextResultOptions {
                path: PathBuf::from("note.txt"),
                content: "resolved\n".to_string(),
                expected_state_token: "stale".to_string(),
            })
            .unwrap_err();
        assert_eq!(stale.code(), SdkErrorCode::RepositoryStale);

        let resolved = session
            .write_and_stage_text_result(&WriteAndStageTextResultOptions {
                path: PathBuf::from("note.txt"),
                content: "resolved\n".to_string(),
                expected_state_token: reopened_token,
            })
            .unwrap();
        let MergeStatus::Merging {
            unmerged_count,
            state_token: resolved_token,
            ..
        } = resolved.merge
        else {
            panic!("expected merge state until continue");
        };
        assert_eq!(unmerged_count, 0);
        assert_eq!(fs::read_to_string(&note).unwrap(), "resolved\n");

        let completed = session
            .continue_merge(&ContinueMergeOptions {
                message: "merge hosted".to_string(),
                expected_state_token: resolved_token,
            })
            .unwrap();
        assert_eq!(completed.merge, MergeStatus::None);
        assert!(
            !directory
                .path()
                .join(".graft/merge-resolution-session.json")
                .exists()
        );
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        let commit = repo
            .read_commit(&repo.head_target().unwrap().unwrap())
            .unwrap();
        assert_eq!(commit.parents.len(), 2);
        assert_eq!(fs::read_to_string(note).unwrap(), "resolved\n");
    }

    #[test]
    fn merge_path_result_selects_ours_and_theirs() {
        for (result, expected) in [
            (MergePathResult::Ours, "local\n"),
            (MergePathResult::Theirs, "hosted\n"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let note = directory.path().join("note.txt");
            let repo = Repository::init(directory.path()).unwrap();

            fs::write(&note, "base\n").unwrap();
            repo.stage_artifact_path(&note).unwrap();
            repo.commit_staged("base").unwrap();
            repo.switch_new_branch("hosted", None).unwrap();
            fs::write(&note, "hosted\n").unwrap();
            repo.stage_artifact_path(&note).unwrap();
            let theirs = repo.commit_staged("hosted edit").unwrap();
            repo.switch_branch("main").unwrap();
            fs::write(&note, "local\n").unwrap();
            repo.stage_artifact_path(&note).unwrap();
            let ours = repo.commit_staged("local edit").unwrap();
            drop(repo);

            let session = RepositorySession::new(directory.path());
            session.open().unwrap();
            let plan = session
                .plan_merge(&PlanMergeOptions {
                    revision: theirs.id.clone(),
                    expected_head: Some(ours.id.clone()),
                })
                .unwrap();
            let applied = session
                .apply_merge(&ApplyMergeOptions {
                    revision: theirs.id,
                    expected_head: Some(ours.id),
                    plan_token: plan.plan_token,
                })
                .unwrap();
            let MergeStatus::Merging { state_token, .. } = applied.merge else {
                panic!("expected an active merge");
            };

            let resolved = session
                .set_merge_path_result(&SetMergePathResultOptions {
                    path: PathBuf::from("note.txt"),
                    result,
                    expected_state_token: state_token.clone(),
                })
                .unwrap();
            let MergeStatus::Merging {
                unmerged_count,
                state_token: resolved_token,
                ..
            } = resolved.merge
            else {
                panic!("expected merge state until continue or abort");
            };
            assert_eq!(unmerged_count, 0);
            assert_ne!(resolved_token, state_token);
            assert_eq!(fs::read_to_string(&note).unwrap(), expected);

            let paths = session
                .list_merge_paths(&ListMergePathsOptions {
                    filter: MergePathFilter::Resolved,
                    limit: 10,
                    after: None,
                    expected_state_token: resolved_token.clone(),
                })
                .unwrap();
            assert_eq!(paths.items.len(), 1);
            assert_eq!(paths.items[0].state, MergePathState::Resolved);

            let resolved_conflicts = session
                .list_merge_conflicts(&ListMergeConflictsOptions {
                    path: PathBuf::from("note.txt"),
                    limit: 10,
                    after: None,
                    expected_state_token: resolved_token.clone(),
                })
                .unwrap();
            assert_eq!(resolved_conflicts.items.len(), 1);
            assert_eq!(resolved_conflicts.items[0]["status"], "resolved");
            assert_eq!(
                resolved_conflicts.items[0]["resolution"],
                match result {
                    MergePathResult::Ours => "ours",
                    MergePathResult::Theirs => "theirs",
                }
            );

            let stale = session
                .unresolve_merge_path(&UnresolveMergePathOptions {
                    path: PathBuf::from("note.txt"),
                    expected_state_token: state_token,
                })
                .unwrap_err();
            assert_eq!(stale.code(), SdkErrorCode::RepositoryStale);
            assert_eq!(fs::read_to_string(&note).unwrap(), expected);

            let unresolved = session
                .unresolve_merge_path(&UnresolveMergePathOptions {
                    path: PathBuf::from("note.txt"),
                    expected_state_token: resolved_token,
                })
                .unwrap();
            let MergeStatus::Merging {
                unmerged_count,
                state_token: unresolved_token,
                ..
            } = unresolved.merge
            else {
                panic!("expected active merge after unresolve");
            };
            assert_eq!(unmerged_count, 1);
            assert_eq!(fs::read_to_string(&note).unwrap(), "local\n");
            let index = Repository::open(directory.path())
                .unwrap()
                .read_index()
                .unwrap();
            assert_eq!(index.conflicted_paths(), vec!["note.txt"]);
            let conflicts = session
                .list_merge_conflicts(&ListMergeConflictsOptions {
                    path: PathBuf::from("note.txt"),
                    limit: 10,
                    after: None,
                    expected_state_token: unresolved_token.clone(),
                })
                .unwrap();
            assert_eq!(conflicts.items[0]["status"], "unresolved");
            assert!(conflicts.items[0].get("resolution").is_none());

            session
                .abort_merge(&AbortMergeOptions { expected_state_token: unresolved_token })
                .unwrap();
            assert_eq!(fs::read_to_string(&note).unwrap(), "local\n");
            assert!(
                !directory
                    .path()
                    .join(".graft/merge-resolution-session.json")
                    .exists()
            );
            session.close().unwrap();
        }
    }

    #[test]
    fn sqlite_merge_selecting_ours_does_not_record_a_physical_only_change() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("space.eidos");
        {
            let database = Connection::open(&database_path).unwrap();
            database
                .execute_batch(
                    "CREATE TABLE docs (id INTEGER PRIMARY KEY, value TEXT NOT NULL);\
                     INSERT INTO docs VALUES (1, 'base');",
                )
                .unwrap();
        }

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        session.commit("base").unwrap();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_new_branch("hosted", None).unwrap();
        drop(repo);
        Connection::open(&database_path)
            .unwrap()
            .execute("UPDATE docs SET value = 'hosted' WHERE id = 1", [])
            .unwrap();
        session.open().unwrap();
        session.add_all().unwrap();
        let hosted = session.commit("hosted row").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_branch("main").unwrap();
        drop(repo);
        Connection::open(&database_path)
            .unwrap()
            .execute("UPDATE docs SET value = 'local' WHERE id = 1", [])
            .unwrap();
        session.open().unwrap();
        session.add_all().unwrap();
        let local = session.commit("local row").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let plan = session
            .plan_merge(&PlanMergeOptions {
                revision: hosted.clone(),
                expected_head: Some(local.clone()),
            })
            .unwrap();
        let applied = session
            .apply_merge(&ApplyMergeOptions {
                revision: hosted,
                expected_head: Some(local),
                plan_token: plan.plan_token,
            })
            .unwrap();
        let MergeStatus::Merging { state_token, .. } = applied.merge else {
            panic!("expected SQLite merge conflict");
        };
        let resolved = session
            .resolve_merge_row(&ResolveMergeRowOptions {
                path: PathBuf::from("space.eidos"),
                table: "docs".to_string(),
                identity: json!(1),
                result: MergePathResult::Ours,
                expected_state_token: state_token,
            })
            .unwrap();
        let MergeStatus::Merging {
            unmerged_count,
            state_token: resolved_token,
            ..
        } = resolved.merge
        else {
            panic!("expected resolved active merge");
        };
        assert_eq!(unmerged_count, 0);
        assert!(resolved.worktree_paths.is_empty());

        let completed = session
            .continue_merge(&ContinueMergeOptions {
                message: "merge hosted".to_string(),
                expected_state_token: resolved_token,
            })
            .unwrap();
        let merge_commit = completed.output["commit"]["id"].as_str().unwrap();
        let changed = session
            .commit_changed_paths(&CommitChangedPathsOptions {
                revision: merge_commit.to_string(),
                limit: 100,
                after: None,
            })
            .unwrap();
        assert!(
            changed.paths.is_empty(),
            "selecting an unchanged first-parent SQLite result must not create a history-only modification: {:?}",
            changed.paths
        );
        session.close().unwrap();
    }

    #[test]
    fn semantic_merge_workspace_survives_reopen_and_stages_validated_result() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("space.eidos");
        {
            let database = Connection::open(&database_path).unwrap();
            database
                .execute_batch(
                    "CREATE TABLE docs (id INTEGER PRIMARY KEY, value TEXT NOT NULL);\
                     INSERT INTO docs VALUES (1, 'base');\
                     CREATE TABLE user_rows (id INTEGER PRIMARY KEY, value TEXT NOT NULL);\
                     INSERT INTO user_rows VALUES (1, 'base');",
                )
                .unwrap();
        }

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        session.commit("base").unwrap();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_new_branch("hosted", None).unwrap();
        drop(repo);
        Connection::open(&database_path)
            .unwrap()
            .execute_batch(
                "UPDATE docs SET value = 'hosted' WHERE id = 1;\
                 UPDATE user_rows SET value = 'hosted' WHERE id = 1;",
            )
            .unwrap();
        session.open().unwrap();
        session.add_all().unwrap();
        let hosted = session.commit("hosted row").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_branch("main").unwrap();
        drop(repo);
        Connection::open(&database_path)
            .unwrap()
            .execute_batch(
                "UPDATE docs SET value = 'local' WHERE id = 1;\
                 UPDATE user_rows SET value = 'base' WHERE id = 1;",
            )
            .unwrap();
        session.open().unwrap();
        session.add_all().unwrap();
        let local = session.commit("local row").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let plan = session
            .plan_merge(&PlanMergeOptions {
                revision: hosted.clone(),
                expected_head: Some(local.clone()),
            })
            .unwrap();
        let applied = session
            .apply_merge(&ApplyMergeOptions {
                revision: hosted,
                expected_head: Some(local),
                plan_token: plan.plan_token,
            })
            .unwrap();
        let MergeStatus::Merging { state_token, .. } = applied.merge else {
            panic!("expected SQLite merge conflict");
        };

        let prepared = session
            .prepare_semantic_merge(&PrepareSemanticMergeOptions {
                path: PathBuf::from("space.eidos"),
                provider: "test.system-merge-1.0".to_string(),
                managed_tables: vec!["docs".to_string()],
                expected_state_token: state_token.clone(),
            })
            .unwrap();
        assert_eq!(prepared.inputs.len(), 3);
        assert!(
            prepared
                .inputs
                .iter()
                .all(|input| input.file_path.is_some())
        );
        assert!(matches!(
            prepared.record,
            SemanticMergeProviderRecord::Pending
        ));
        assert!(Path::new(&prepared.result_path).is_file());
        assert_eq!(prepared.managed_tables, vec!["docs"]);
        assert_eq!(prepared.managed_conflicts, 1);
        assert!(prepared.prepared_at_unix_ms > 0);
        assert!(prepared.seed_applied_sql);
        let seed = Connection::open(&prepared.result_path).unwrap();
        assert_eq!(
            seed.query_row("SELECT value FROM docs WHERE id = 1", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "local"
        );
        assert_eq!(
            seed.query_row("SELECT value FROM user_rows WHERE id = 1", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "hosted"
        );
        drop(seed);
        let seeded_result = fs::read(&prepared.result_path).unwrap();

        let recorded = session
            .record_semantic_merge_conflicts(&RecordSemanticMergeConflictsOptions {
                provider_token: prepared.provider_token.clone(),
                conflicts: vec![json!({"code": "domain-conflict", "object": "docs/1"})],
                automatic_resolutions: vec![json!({"group": "metadata-clock"})],
                expected_state_token: state_token.clone(),
            })
            .unwrap();
        assert!(matches!(
            recorded.record,
            SemanticMergeProviderRecord::Conflict { ref conflicts, .. } if conflicts.len() == 1
        ));

        session.close().unwrap();
        session.open().unwrap();
        let reopened = session
            .prepare_semantic_merge(&PrepareSemanticMergeOptions {
                path: PathBuf::from("space.eidos"),
                provider: "test.system-merge-1.0".to_string(),
                managed_tables: vec!["docs".to_string()],
                expected_state_token: state_token.clone(),
            })
            .unwrap();
        assert_eq!(reopened.provider_token, prepared.provider_token);
        assert!(matches!(
            reopened.record,
            SemanticMergeProviderRecord::Conflict { .. }
        ));

        let ours = reopened
            .inputs
            .iter()
            .find(|input| input.version == MergeSqliteVersion::Ours)
            .and_then(|input| input.file_path.as_deref())
            .unwrap();
        assert!(Path::new(ours).is_file());
        fs::write(&reopened.result_path, b"not a sqlite database").unwrap();
        assert!(
            session
                .accept_semantic_merge_result(&AcceptSemanticMergeResultOptions {
                    provider_token: reopened.provider_token.clone(),
                    validation: json!({"profile": "test", "valid": true}),
                    automatic_resolutions: Vec::new(),
                    expected_state_token: state_token.clone(),
                })
                .is_err()
        );
        let after_failed_accept = session
            .prepare_semantic_merge(&PrepareSemanticMergeOptions {
                path: PathBuf::from("space.eidos"),
                provider: "test.system-merge-1.0".to_string(),
                managed_tables: vec!["docs".to_string()],
                expected_state_token: state_token.clone(),
            })
            .unwrap();
        assert!(matches!(
            after_failed_accept.record,
            SemanticMergeProviderRecord::Conflict { .. }
        ));
        assert!(matches!(
            session.get_merge_status().unwrap(),
            MergeStatus::Merging { unmerged_count: 1, .. }
        ));
        fs::write(&reopened.result_path, seeded_result).unwrap();
        Connection::open(&reopened.result_path)
            .unwrap()
            .execute("UPDATE docs SET value = 'semantic' WHERE id = 1", [])
            .unwrap();

        let accepted = session
            .accept_semantic_merge_result(&AcceptSemanticMergeResultOptions {
                provider_token: reopened.provider_token,
                validation: json!({"profile": "test", "valid": true}),
                automatic_resolutions: vec![json!({"group": "docs/1"})],
                expected_state_token: state_token,
            })
            .unwrap();
        assert_eq!(accepted.worktree_paths, vec!["space.eidos"]);
        let MergeStatus::Merging {
            state_token: accepted_token,
            unmerged_count,
            ..
        } = accepted.merge
        else {
            panic!("expected resolved merge pending continue");
        };
        assert_eq!(unmerged_count, 0);
        assert_eq!(
            Connection::open(&database_path)
                .unwrap()
                .query_row("SELECT value FROM docs WHERE id = 1", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "semantic"
        );

        let completed = session
            .continue_merge(&ContinueMergeOptions {
                message: "semantic merge".to_string(),
                expected_state_token: accepted_token,
            })
            .unwrap();
        assert_eq!(completed.merge, MergeStatus::None);
        assert!(!directory.path().join(".graft/semantic-merge").exists());
        session.close().unwrap();
    }

    #[test]
    fn sqlite_row_resolutions_survive_reopen_and_keep_both_selected_sides() {
        let root = tempfile::tempdir().unwrap();
        let source_directory = root.path().join("source");
        let remote_directory = root.path().join("remote");
        let decoy_remote_directory = root.path().join("decoy-remote");
        let clone_directory = root.path().join("clone");
        fs::create_dir_all(&source_directory).unwrap();
        fs::create_dir_all(&remote_directory).unwrap();
        fs::create_dir_all(&decoy_remote_directory).unwrap();
        fs::create_dir_all(&clone_directory).unwrap();
        let source_database = source_directory.join("space.eidos");
        let clone_database = clone_directory.join("space.eidos");

        {
            let database = Connection::open(&source_database).unwrap();
            database
                .execute_batch(
                    "CREATE TABLE docs (id INTEGER PRIMARY KEY, value TEXT NOT NULL);\
                     INSERT INTO docs VALUES (1, 'base-one'), (2, 'base-two'), (3, 'base-three');\
                     CREATE TABLE tasks (id INTEGER PRIMARY KEY, value TEXT NOT NULL);\
                     INSERT INTO tasks VALUES (1, 'base-task'), (2, 'base-task-two');",
                )
                .unwrap();
        }

        let source = RepositorySession::new(&source_directory);
        source.open().unwrap();
        source.init().unwrap();
        source.add_all().unwrap();
        source.commit("base").unwrap();
        source
            .configure_remote(&RemoteConfigureOptions {
                name: "origin".to_string(),
                url: format!("fs://{}", remote_directory.display()),
                bearer_token: None,
                overwrite: false,
                upstream_branch: Some("main".to_string()),
            })
            .unwrap();
        source.push(None, None).unwrap();

        let clone = RepositorySession::new(&clone_directory);
        clone.open().unwrap();
        clone
            .clone_repository(&format!("fs://{}", remote_directory.display()), None, None)
            .unwrap();
        clone
            .configure_remote(&RemoteConfigureOptions {
                name: "origin".to_string(),
                url: format!("fs://{}", decoy_remote_directory.display()),
                bearer_token: None,
                overwrite: true,
                upstream_branch: Some("main".to_string()),
            })
            .unwrap();
        clone
            .configure_remote(&RemoteConfigureOptions {
                name: "backup".to_string(),
                url: format!("fs://{}", remote_directory.display()),
                bearer_token: None,
                overwrite: false,
                upstream_branch: None,
            })
            .unwrap();

        {
            let database = Connection::open(&source_database).unwrap();
            database
                .execute_batch(
                    "UPDATE docs SET value = 'hosted-one' WHERE id = 1;\
                     UPDATE docs SET value = 'hosted-two' WHERE id = 2;\
                     UPDATE docs SET value = 'hosted-three' WHERE id = 3;\
                     UPDATE tasks SET value = 'hosted-task' WHERE id = 1;",
                )
                .unwrap();
        }
        source.add_all().unwrap();
        source.commit("hosted rows").unwrap();
        source.push(None, None).unwrap();

        {
            let database = Connection::open(&clone_database).unwrap();
            database
                .execute_batch(
                    "UPDATE docs SET value = 'local-one' WHERE id = 1;\
                     UPDATE docs SET value = 'local-two' WHERE id = 2;\
                     UPDATE tasks SET value = 'local-task' WHERE id = 1;\
                     UPDATE tasks SET value = 'local-task-two' WHERE id = 2;",
                )
                .unwrap();
        }
        clone.add_all().unwrap();
        let local_head = clone.commit("local rows").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        clone.fetch(Some("backup"), Some("main")).unwrap();

        let plan = clone
            .plan_merge(&PlanMergeOptions {
                revision: "backup/main".to_string(),
                expected_head: Some(local_head.clone()),
            })
            .unwrap();
        assert_eq!(plan.kind, MergePlanKind::ThreeWay);
        assert_eq!(plan.conflicted_paths, vec!["space.eidos"]);
        let applied = clone
            .apply_merge(&ApplyMergeOptions {
                revision: "backup/main".to_string(),
                expected_head: Some(local_head),
                plan_token: plan.plan_token,
            })
            .unwrap();
        let MergeStatus::Merging { state_token, unmerged_count, .. } = applied.merge else {
            panic!("expected SQLite merge conflict");
        };
        assert_eq!(unmerged_count, 1);

        let conflicts = clone
            .list_merge_conflicts(&ListMergeConflictsOptions {
                path: PathBuf::from("space.eidos"),
                limit: 10,
                after: None,
                expected_state_token: state_token.clone(),
            })
            .unwrap();
        assert_eq!(conflicts.items.len(), 3);
        assert!(conflicts.items.iter().all(|item| item["kind"] == "row"));
        assert!(
            conflicts
                .items
                .iter()
                .all(|item| item["status"] == "unresolved")
        );

        let stale_table = clone
            .resolve_merge_table(&ResolveMergeTableOptions {
                path: PathBuf::from("space.eidos"),
                table: "docs".to_string(),
                result: MergePathResult::Ours,
                expected_state_token: "stale".to_string(),
            })
            .unwrap_err();
        assert_eq!(stale_table.code(), SdkErrorCode::RepositoryStale);
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let cancellation_error = with_cancellation(&cancelled, || {
            clone.resolve_merge_table(&ResolveMergeTableOptions {
                path: PathBuf::from("space.eidos"),
                table: "docs".to_string(),
                result: MergePathResult::Ours,
                expected_state_token: state_token.clone(),
            })
        })
        .unwrap_err();
        assert_eq!(cancellation_error.code(), SdkErrorCode::Cancelled);
        let MergeStatus::Merging { state_token: unchanged_token, .. } =
            clone.get_merge_status().unwrap()
        else {
            panic!("expected unchanged active merge after cancellation");
        };
        assert_eq!(unchanged_token, state_token);

        let first = clone
            .resolve_merge_row(&ResolveMergeRowOptions {
                path: PathBuf::from("space.eidos"),
                table: "docs".to_string(),
                identity: json!(1),
                result: MergePathResult::Ours,
                expected_state_token: state_token,
            })
            .unwrap();
        let MergeStatus::Merging {
            state_token: partial_token,
            unmerged_count,
            ..
        } = first.merge
        else {
            panic!("expected the second row conflict to remain");
        };
        assert_eq!(unmerged_count, 1);
        assert!(first.worktree_paths.is_empty());

        clone.close().unwrap();
        clone.open().unwrap();
        let MergeStatus::Merging { state_token: reopened_token, .. } =
            clone.get_merge_status().unwrap()
        else {
            panic!("expected durable row resolution state");
        };
        assert_eq!(reopened_token, partial_token);
        let partial = clone
            .list_merge_conflicts(&ListMergeConflictsOptions {
                path: PathBuf::from("space.eidos"),
                limit: 10,
                after: None,
                expected_state_token: reopened_token.clone(),
            })
            .unwrap();
        assert_eq!(
            partial
                .items
                .iter()
                .filter(|item| item["status"] == "resolved")
                .count(),
            1
        );

        let second = clone
            .resolve_merge_row(&ResolveMergeRowOptions {
                path: PathBuf::from("space.eidos"),
                table: "docs".to_string(),
                identity: json!(2),
                result: MergePathResult::Theirs,
                expected_state_token: reopened_token,
            })
            .unwrap();
        let MergeStatus::Merging {
            state_token: resolved_token,
            unmerged_count,
            ..
        } = second.merge
        else {
            panic!("expected merge state until continue");
        };
        assert_eq!(unmerged_count, 1);
        assert!(second.worktree_paths.is_empty());
        {
            let database = Connection::open(&clone_database).unwrap();
            let rows = database
                .prepare("SELECT id, value FROM docs ORDER BY id")
                .unwrap()
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(
                rows,
                vec![
                    (1, "local-one".to_string()),
                    (2, "local-two".to_string()),
                    (3, "base-three".to_string()),
                ]
            );
        }

        let table_resolved = clone
            .resolve_merge_table(&ResolveMergeTableOptions {
                path: PathBuf::from("space.eidos"),
                table: "tasks".to_string(),
                result: MergePathResult::Theirs,
                expected_state_token: resolved_token,
            })
            .unwrap();
        let MergeStatus::Merging {
            state_token: table_token, unmerged_count, ..
        } = table_resolved.merge
        else {
            panic!("expected merge state after table resolution");
        };
        assert_eq!(unmerged_count, 0);
        assert_eq!(table_resolved.worktree_paths, vec!["space.eidos"]);

        clone.close().unwrap();
        clone.open().unwrap();
        let MergeStatus::Merging { state_token: reopened_final_token, .. } =
            clone.get_merge_status().unwrap()
        else {
            panic!("expected durable fully-resolved merge");
        };
        assert_eq!(reopened_final_token, table_token);
        let resolved_conflicts = clone
            .list_merge_conflicts(&ListMergeConflictsOptions {
                path: PathBuf::from("space.eidos"),
                limit: 10,
                after: None,
                expected_state_token: reopened_final_token.clone(),
            })
            .unwrap();
        assert_eq!(resolved_conflicts.items.len(), 3);
        assert!(
            resolved_conflicts
                .items
                .iter()
                .all(|item| item["status"] == "resolved")
        );
        assert_eq!(
            resolved_conflicts
                .items
                .iter()
                .find(|item| item["table"] == "tasks")
                .unwrap()["resolution"],
            "theirs"
        );

        let unresolved = clone
            .unresolve_merge_path(&UnresolveMergePathOptions {
                path: PathBuf::from("space.eidos"),
                expected_state_token: reopened_final_token,
            })
            .unwrap();
        let MergeStatus::Merging {
            state_token: unresolved_token,
            unmerged_count,
            ..
        } = unresolved.merge
        else {
            panic!("expected merge state after SQLite unresolve");
        };
        assert_eq!(unmerged_count, 1);
        let reset_conflicts = clone
            .list_merge_conflicts(&ListMergeConflictsOptions {
                path: PathBuf::from("space.eidos"),
                limit: 10,
                after: None,
                expected_state_token: unresolved_token.clone(),
            })
            .unwrap();
        assert!(
            reset_conflicts
                .items
                .iter()
                .all(|item| item["status"] == "unresolved")
        );

        let docs_ours = clone
            .resolve_merge_table(&ResolveMergeTableOptions {
                path: PathBuf::from("space.eidos"),
                table: "docs".to_string(),
                result: MergePathResult::Ours,
                expected_state_token: unresolved_token,
            })
            .unwrap();
        let MergeStatus::Merging { state_token: docs_ours_token, .. } = docs_ours.merge else {
            panic!("expected tasks conflict to remain");
        };
        let docs_theirs = clone
            .resolve_merge_table(&ResolveMergeTableOptions {
                path: PathBuf::from("space.eidos"),
                table: "docs".to_string(),
                result: MergePathResult::Theirs,
                expected_state_token: docs_ours_token,
            })
            .unwrap();
        let MergeStatus::Merging { state_token: docs_theirs_token, .. } = docs_theirs.merge else {
            panic!("expected tasks conflict to remain after switching docs");
        };
        let tasks_ours = clone
            .resolve_merge_table(&ResolveMergeTableOptions {
                path: PathBuf::from("space.eidos"),
                table: "tasks".to_string(),
                result: MergePathResult::Ours,
                expected_state_token: docs_theirs_token,
            })
            .unwrap();
        let MergeStatus::Merging {
            state_token: final_token, unmerged_count, ..
        } = tasks_ours.merge
        else {
            panic!("expected active fully-resolved merge");
        };
        assert_eq!(unmerged_count, 0);
        {
            let database = Connection::open(&clone_database).unwrap();
            let docs = database
                .prepare("SELECT value FROM docs ORDER BY id")
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            let tasks = database
                .prepare("SELECT value FROM tasks ORDER BY id")
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(docs, vec!["hosted-one", "hosted-two", "hosted-three"]);
            assert_eq!(tasks, vec!["local-task", "local-task-two"]);
        }

        let completed = clone
            .continue_merge(&ContinueMergeOptions {
                message: "merge selected rows".to_string(),
                expected_state_token: final_token,
            })
            .unwrap();
        assert_eq!(completed.merge, MergeStatus::None);
        let commit_id = completed.output["commit"]["id"].as_str().unwrap();
        assert_eq!(
            clone.commit_details(commit_id).unwrap()["parents"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(
            !clone_directory
                .join(".graft/merge-resolution-session.json")
                .exists()
        );
        clone.close().unwrap();
        source.close().unwrap();
    }

    #[test]
    fn physical_only_sqlite_side_auto_resolves_to_ours_and_remains_inspectable() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("space.eidos");
        {
            let database = Connection::open(&database_path).unwrap();
            database
                .execute_batch(
                    "CREATE TABLE docs (id INTEGER PRIMARY KEY, value TEXT NOT NULL);\
                     WITH RECURSIVE seq(id) AS (VALUES(1) UNION ALL SELECT id + 1 FROM seq WHERE id < 1000)\
                     INSERT INTO docs SELECT id, printf('%08d-%s', id, hex(zeroblob(512))) FROM seq;\
                     DELETE FROM docs WHERE id % 2 = 0;",
                )
                .unwrap();
        }

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        let base = session.commit("fragmented base").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_new_branch("hosted", None).unwrap();
        drop(repo);
        {
            let database = Connection::open(&database_path).unwrap();
            database.execute_batch("VACUUM;").unwrap();
        }
        session.open().unwrap();
        session.add_all().unwrap();
        let theirs = session.commit("physical compaction").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_branch("main").unwrap();
        drop(repo);
        {
            let database = Connection::open(&database_path).unwrap();
            database
                .execute("UPDATE docs SET value = 'local' WHERE id = 1", [])
                .unwrap();
        }
        session.open().unwrap();
        session.add_all().unwrap();
        let ours = session.commit("local row").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let plan = session
            .plan_merge(&PlanMergeOptions {
                revision: theirs.clone(),
                expected_head: Some(ours.clone()),
            })
            .unwrap();
        assert_eq!(plan.merge_base.as_deref(), Some(base.as_str()));
        assert_eq!(plan.conflicted_paths, vec!["space.eidos"]);
        let applied = session
            .apply_merge(&ApplyMergeOptions {
                revision: theirs,
                expected_head: Some(ours),
                plan_token: plan.plan_token,
            })
            .unwrap();
        let MergeStatus::Merging { unmerged_count, state_token, .. } = applied.merge else {
            panic!("expected active merge");
        };
        assert_eq!(unmerged_count, 0);
        assert_eq!(
            Connection::open(&database_path)
                .unwrap()
                .query_row("SELECT value FROM docs WHERE id = 1", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "local"
        );

        let unresolved = session
            .unresolve_merge_path(&UnresolveMergePathOptions {
                path: PathBuf::from("space.eidos"),
                expected_state_token: state_token,
            })
            .unwrap();
        let MergeStatus::Merging {
            unmerged_count,
            state_token: unresolved_token,
            ..
        } = unresolved.merge
        else {
            panic!("expected unresolved merge");
        };
        assert_eq!(unmerged_count, 1);
        let conflicts = session
            .list_merge_conflicts(&ListMergeConflictsOptions {
                path: PathBuf::from("space.eidos"),
                limit: 10,
                after: None,
                expected_state_token: unresolved_token.clone(),
            })
            .unwrap();
        assert_eq!(conflicts.items.len(), 1);
        assert_eq!(
            conflicts.items[0]["reason"],
            "theirs_logically_equivalent_to_base"
        );
        assert_eq!(conflicts.items[0]["auto_resolvable"], true);
        assert_eq!(conflicts.items[0]["recommended_result"], "ours");

        let diff = session
            .diff_merge_sqlite(&DiffMergeSqliteOptions {
                path: PathBuf::from("space.eidos"),
                from: MergeSqliteVersion::Base,
                to: MergeSqliteVersion::Theirs,
                response: SqliteDiffResponse::Summary,
                expected_state_token: unresolved_token.clone(),
            })
            .unwrap();
        assert_eq!(diff.state_token, unresolved_token);
        assert_eq!(diff.from.version, MergeSqliteVersion::Base);
        assert_eq!(diff.to.version, MergeSqliteVersion::Theirs);
        assert_eq!(
            diff.diff["files"][0]["logical_status"],
            "file_changed_no_supported_logical_changes"
        );
        assert!(diff.diff["files"][0].get("schema_changes").is_none());

        let stale = session
            .diff_merge_sqlite(&DiffMergeSqliteOptions {
                path: PathBuf::from("space.eidos"),
                from: MergeSqliteVersion::Base,
                to: MergeSqliteVersion::Ours,
                response: SqliteDiffResponse::Summary,
                expected_state_token: "stale".to_string(),
            })
            .unwrap_err();
        assert_eq!(stale.code(), SdkErrorCode::RepositoryStale);
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let error = with_cancellation(&cancelled, || {
            session.diff_merge_sqlite(&DiffMergeSqliteOptions {
                path: PathBuf::from("space.eidos"),
                from: MergeSqliteVersion::Base,
                to: MergeSqliteVersion::Ours,
                response: SqliteDiffResponse::Summary,
                expected_state_token: unresolved_token.clone(),
            })
        })
        .unwrap_err();
        assert_eq!(error.code(), SdkErrorCode::Cancelled);
        let MergeStatus::Merging { state_token: unchanged, .. } =
            session.get_merge_status().unwrap()
        else {
            panic!("expected unchanged merge");
        };
        assert_eq!(unchanged, unresolved_token);

        session
            .abort_merge(&AbortMergeOptions { expected_state_token: unresolved_token })
            .unwrap();
        session.close().unwrap();
    }

    #[test]
    fn table_resolution_rejects_schema_opaque_and_semantic_key_conflicts_without_changes() {
        let directory = tempfile::tempdir().unwrap();
        let schema_database = directory.path().join("schema.eidos");
        let opaque_database = directory.path().join("opaque.eidos");
        let semantic_database = directory.path().join("semantic.eidos");
        let rewrite = |path: &Path, sql: &str| {
            for candidate in [
                path.to_path_buf(),
                PathBuf::from(format!("{}-wal", path.display())),
                PathBuf::from(format!("{}-shm", path.display())),
            ] {
                let _ = fs::remove_file(candidate);
            }
            let database = Connection::open(path).unwrap();
            database.execute_batch(sql).unwrap();
        };

        rewrite(
            &schema_database,
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, value TEXT NOT NULL);",
        );
        rewrite(
            &opaque_database,
            "CREATE VIRTUAL TABLE search USING fts5(content);\
             INSERT INTO search VALUES ('base');",
        );
        rewrite(
            &semantic_database,
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, value TEXT NOT NULL);",
        );

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        session.commit("base").unwrap();
        session.close().unwrap();
        let repo = Repository::open(directory.path()).unwrap();
        repo.config_set("merge.semantic_keys.docs", "slug").unwrap();
        repo.switch_new_branch("hosted", None).unwrap();
        drop(repo);

        rewrite(
            &schema_database,
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, value TEXT NOT NULL, branch TEXT);",
        );
        rewrite(
            &opaque_database,
            "CREATE VIRTUAL TABLE search USING fts5(content);\
             INSERT INTO search VALUES ('hosted');",
        );
        rewrite(
            &semantic_database,
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, value TEXT NOT NULL);\
             INSERT INTO docs VALUES (1, 'same', 'hosted');",
        );
        session.open().unwrap();
        session.add_all().unwrap();
        let hosted = session.commit("hosted conflicts").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        session.close().unwrap();
        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_branch("main").unwrap();
        drop(repo);

        rewrite(
            &schema_database,
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, value TEXT NOT NULL, branch INTEGER);",
        );
        rewrite(
            &opaque_database,
            "CREATE VIRTUAL TABLE search USING fts5(content);\
             INSERT INTO search VALUES ('local');",
        );
        rewrite(
            &semantic_database,
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, value TEXT NOT NULL);\
             INSERT INTO docs VALUES (2, 'same', 'local');",
        );
        session.open().unwrap();
        session.add_all().unwrap();
        let local = session.commit("local conflicts").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let plan = session
            .plan_merge(&PlanMergeOptions {
                revision: hosted.clone(),
                expected_head: Some(local.clone()),
            })
            .unwrap();
        let applied = session
            .apply_merge(&ApplyMergeOptions {
                revision: hosted,
                expected_head: Some(local),
                plan_token: plan.plan_token,
            })
            .unwrap();
        let MergeStatus::Merging { state_token, .. } = applied.merge else {
            panic!("expected active merge");
        };
        let before = [
            fs::read(&schema_database).unwrap(),
            fs::read(&opaque_database).unwrap(),
            fs::read(&semantic_database).unwrap(),
        ];

        let schema_conflicts = session
            .list_merge_conflicts(&ListMergeConflictsOptions {
                path: PathBuf::from("schema.eidos"),
                limit: 10,
                after: None,
                expected_state_token: state_token.clone(),
            })
            .unwrap();
        assert_eq!(schema_conflicts.items.len(), 1);
        assert_eq!(schema_conflicts.items[0]["kind"], "schema");
        assert_eq!(schema_conflicts.items[0]["name"], "docs");
        assert_eq!(schema_conflicts.items[0]["entry_type"], "table");
        assert_eq!(schema_conflicts.items[0]["ours_op"], "modified");
        assert_eq!(schema_conflicts.items[0]["theirs_op"], "modified");
        assert!(
            !schema_conflicts.items[0]["column_changes"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let schema_diff = session
            .diff_merge_sqlite(&DiffMergeSqliteOptions {
                path: PathBuf::from("schema.eidos"),
                from: MergeSqliteVersion::Base,
                to: MergeSqliteVersion::Ours,
                response: SqliteDiffResponse::Summary,
                expected_state_token: state_token.clone(),
            })
            .unwrap();
        assert_eq!(schema_diff.state_token, state_token);
        assert_eq!(
            schema_diff.diff["files"][0]["schema_changes"][0]["name"],
            "docs"
        );
        assert_eq!(
            schema_diff.diff["files"][0]["schema_changes"][0]["entry_type"],
            "table"
        );
        assert_eq!(
            schema_diff.diff["files"][0]["schema_changes"][0]["op"],
            "modified"
        );
        assert!(
            schema_diff.diff["files"][0]["schema_changes"][0]["sql"]
                .as_str()
                .unwrap()
                .contains("branch INTEGER")
        );

        for (path, table, expected_message) in [
            ("schema.eidos", "docs", "schema conflicts"),
            ("opaque.eidos", "search", "opaque conflicts"),
            ("semantic.eidos", "docs", "semantic key conflict"),
        ] {
            let error = session
                .resolve_merge_table(&ResolveMergeTableOptions {
                    path: PathBuf::from(path),
                    table: table.to_string(),
                    result: MergePathResult::Ours,
                    expected_state_token: state_token.clone(),
                })
                .unwrap_err();
            assert_eq!(error.code(), SdkErrorCode::RepositoryCommand);
            assert!(
                error.message().contains(expected_message),
                "{}",
                error.message()
            );
            let MergeStatus::Merging { state_token: unchanged, .. } =
                session.get_merge_status().unwrap()
            else {
                panic!("expected rejected table resolution to preserve merge");
            };
            assert_eq!(unchanged, state_token);
        }
        assert_eq!(
            before,
            [
                fs::read(&schema_database).unwrap(),
                fs::read(&opaque_database).unwrap(),
                fs::read(&semantic_database).unwrap(),
            ]
        );

        session
            .abort_merge(&AbortMergeOptions { expected_state_token: state_token })
            .unwrap();
        assert!(
            !directory
                .path()
                .join(".graft/merge-resolution-session.json")
                .exists()
        );
        session.close().unwrap();
    }

    #[test]
    fn merge_plan_and_abort_reject_stale_tokens_and_restore_orig_head() {
        let directory = tempfile::tempdir().unwrap();
        let note = directory.path().join("note.txt");
        let repo = Repository::init(directory.path()).unwrap();
        fs::write(&note, "base\n").unwrap();
        repo.stage_artifact_path(&note).unwrap();
        repo.commit_staged("base").unwrap();
        repo.switch_new_branch("hosted", None).unwrap();
        fs::write(&note, "hosted\n").unwrap();
        repo.stage_artifact_path(&note).unwrap();
        let theirs = repo.commit_staged("hosted edit").unwrap();
        repo.switch_branch("main").unwrap();
        fs::write(&note, "local\n").unwrap();
        repo.stage_artifact_path(&note).unwrap();
        let ours = repo.commit_staged("local edit").unwrap();
        drop(repo);

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        let plan = session
            .plan_merge(&PlanMergeOptions {
                revision: theirs.id.clone(),
                expected_head: Some(ours.id.clone()),
            })
            .unwrap();
        let stale_plan = session
            .apply_merge(&ApplyMergeOptions {
                revision: theirs.id.clone(),
                expected_head: Some(ours.id.clone()),
                plan_token: "stale".to_string(),
            })
            .unwrap_err();
        assert_eq!(stale_plan.code(), SdkErrorCode::RepositoryStale);
        let applied = session
            .apply_merge(&ApplyMergeOptions {
                revision: theirs.id,
                expected_head: Some(ours.id.clone()),
                plan_token: plan.plan_token,
            })
            .unwrap();
        let MergeStatus::Merging { state_token, .. } = applied.merge else {
            panic!("expected merge state");
        };
        let stale_abort = session
            .abort_merge(&AbortMergeOptions {
                expected_state_token: "stale".to_string(),
            })
            .unwrap_err();
        assert_eq!(stale_abort.code(), SdkErrorCode::RepositoryStale);
        let aborted = session
            .abort_merge(&AbortMergeOptions { expected_state_token: state_token })
            .unwrap();
        assert_eq!(aborted.merge, MergeStatus::None);
        assert_eq!(fs::read_to_string(&note).unwrap(), "local\n");
        assert_eq!(
            session
                .repository_metadata()
                .unwrap()
                .current_head
                .as_deref(),
            Some(ours.id.as_str())
        );
    }

    #[test]
    fn staged_move_metadata_overlays_unstaged_sqlite_rows() {
        let mut worktree = serde_json::json!({
            "paths": [{ "path": "new.eidos", "change": "modified" }],
            "files": [{
                "path": "new.eidos",
                "change": "modified",
                "tables": [{ "name": "Table 1", "changes": [{ "op": "insert" }] }]
            }]
        });
        let staged = serde_json::json!({
            "paths": [{
                "path": "new.eidos",
                "previous_path": "old.eidos",
                "change": "renamed"
            }]
        });

        overlay_staged_renames(&mut worktree, &staged);

        assert_eq!(worktree["paths"][0]["change"], "renamed");
        assert_eq!(worktree["files"][0]["change"], "renamed");
        assert_eq!(worktree["files"][0]["previous_path"], "old.eidos");
        assert_eq!(
            worktree["files"][0]["tables"][0]["changes"][0]["op"],
            "insert"
        );
    }

    #[test]
    fn publication_outcomes_have_stable_sdk_error_codes() {
        let transport_error = || graft::remote::RemoteErr::HttpStatus {
            status: 503,
            path: "refs/heads/main".to_string(),
            message: "test transport stand-in".to_string(),
        };
        let unconfirmed = repository_command_error(ErrCtx::Repo(graft::repo::RepoErr::Remote(
            graft::remote::RemoteErr::PublicationUnconfirmed {
                path: "refs/heads/main".to_string(),
                source: Box::new(transport_error()),
            },
        )));
        assert_eq!(
            unconfirmed.code(),
            SdkErrorCode::RemotePublicationUnconfirmed
        );
        assert_eq!(
            unconfirmed.code().as_str(),
            "GRAFT_SDK_REMOTE_PUBLICATION_UNCONFIRMED"
        );

        let unknown = repository_command_error(ErrCtx::Repo(graft::repo::RepoErr::Remote(
            graft::remote::RemoteErr::PublicationOutcomeUnknown {
                path: "refs/heads/main".to_string(),
                publication_error: Box::new(transport_error()),
                reconciliation_error: Box::new(transport_error()),
            },
        )));
        assert_eq!(
            unknown.code(),
            SdkErrorCode::RemotePublicationOutcomeUnknown
        );
        assert_eq!(
            unknown.code().as_str(),
            "GRAFT_SDK_REMOTE_PUBLICATION_OUTCOME_UNKNOWN"
        );
    }

    #[test]
    fn session_reuses_runtime_and_reopens_after_close() {
        let directory = tempfile::tempdir().unwrap();
        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();

        fs::write(directory.path().join("note.txt"), "one\n").unwrap();
        session.add_all().unwrap();
        session.commit("initial").unwrap();
        for _ in 0..10 {
            assert_eq!(session.status().unwrap()["dirty"], json!(false));
            session.diff(&DiffOptions::default()).unwrap();
        }

        session.close().unwrap();
        assert_eq!(session.lifecycle(), SessionLifecycle::Closed);
        assert_eq!(
            session.status().unwrap_err().code(),
            SdkErrorCode::SessionClosed
        );
        session.reopen().unwrap();
        assert_eq!(session.status().unwrap()["dirty"], json!(false));
    }

    #[test]
    fn incremental_status_reuses_metadata_and_advances_generation() {
        let directory = tempfile::tempdir().unwrap();
        let note = directory.path().join("note.txt");
        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        fs::write(&note, "one\n").unwrap();
        session.add_all().unwrap();
        session.commit("initial").unwrap();

        let first = session.status_incremental().unwrap();
        assert!(!first.telemetry.status_cache_hit);
        assert!(!first.status.dirty);
        let second = session.status_incremental().unwrap();
        assert!(second.telemetry.status_cache_hit);
        assert_eq!(second.generation, first.generation);
        assert_eq!(second.change_token, first.change_token);

        fs::write(&note, "two\n").unwrap();
        let changed = session.status_incremental().unwrap();
        assert!(!changed.telemetry.status_cache_hit);
        assert!(changed.status.dirty);
        assert!(changed.generation > second.generation);
        let hot = session.status_incremental().unwrap();
        assert!(hot.telemetry.status_cache_hit);
        assert_eq!(hot.generation, changed.generation);
    }

    #[test]
    fn persistent_status_snapshot_survives_reopen_and_invalidates_safely() {
        let directory = tempfile::tempdir().unwrap();
        let note = directory.path().join("note.txt");
        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        fs::write(&note, "one\n").unwrap();
        session.add_all().unwrap();
        session.commit("initial").unwrap();

        let built = session.status_incremental().unwrap();
        assert!(built.telemetry.persistent_snapshot_saved);
        assert!(!built.telemetry.persistent_snapshot_hit);
        let generation = built.generation;
        session.close().unwrap();
        session.open().unwrap();

        let reopened = session.status_incremental().unwrap();
        assert!(reopened.telemetry.persistent_snapshot_hit);
        assert!(reopened.telemetry.status_cache_hit);
        assert_eq!(reopened.generation, generation);
        assert_eq!(reopened.change_token, built.change_token);
        assert!(!reopened.status.dirty);

        session.close().unwrap();
        fs::write(directory.path().join(".gitignore"), "generated/\n").unwrap();
        session.open().unwrap();
        let ignore_changed = session.status_incremental().unwrap();
        assert!(!ignore_changed.telemetry.persistent_snapshot_hit);
        assert!(!ignore_changed.telemetry.status_cache_hit);
        assert!(ignore_changed.status.dirty);
        assert_ne!(ignore_changed.change_token, reopened.change_token);

        session.close().unwrap();
        fs::write(&note, "externally staged\n").unwrap();
        let writer = Repository::open(directory.path()).unwrap();
        writer.stage_artifact_path(&note).unwrap();
        session.open().unwrap();
        let index_changed = session.status_incremental().unwrap();
        assert!(!index_changed.telemetry.persistent_snapshot_hit);
        assert!(index_changed.status.has_staged_changes);

        session.close().unwrap();
        let external_commit = writer.commit("external writer").unwrap();
        session.open().unwrap();
        let head_changed = session.status_incremental().unwrap();
        assert!(!head_changed.telemetry.persistent_snapshot_hit);
        assert_eq!(
            head_changed.status.head_target.as_deref(),
            Some(external_commit.id.as_str())
        );

        session.close().unwrap();
        let mut config = writer.config().unwrap();
        config.track.user_roots.push("docs".to_string());
        writer.write_config(&config).unwrap();
        session.open().unwrap();
        let config_changed = session.status_incremental().unwrap();
        assert!(!config_changed.telemetry.persistent_snapshot_hit);

        let cache_directory = directory.path().join(".graft/cache/sdk-status");
        session.close().unwrap();
        fs::write(
            cache_directory.join(".classification-v1-killed-writer.tmp"),
            b"truncated",
        )
        .unwrap();
        session.open().unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let cancelled =
            with_cancellation(&cancellation, || session.status_incremental()).unwrap_err();
        assert_eq!(cancelled.code(), SdkErrorCode::Cancelled);
        assert!(
            session
                .status_incremental()
                .unwrap()
                .telemetry
                .persistent_snapshot_hit
        );
        let snapshot = fs::read_dir(cache_directory)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .unwrap();
        let encoded = fs::read_to_string(snapshot).unwrap();
        assert!(!encoded.contains(&directory.path().to_string_lossy().to_string()));
    }

    #[test]
    fn remote_tracking_updates_reuse_local_status_proof_and_refresh_projection() {
        let directory = tempfile::tempdir().unwrap();
        let note = directory.path().join("note.txt");
        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();

        fs::write(&note, "one\n").unwrap();
        session.add_all().unwrap();
        let first = session.commit("first").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        session
            .configure_remote(&RemoteConfigureOptions {
                name: "origin".to_string(),
                url: "https://example.invalid/acme/repo".to_string(),
                bearer_token: None,
                overwrite: false,
                upstream_branch: Some("main".to_string()),
            })
            .unwrap();
        let writer = Repository::open(directory.path()).unwrap();
        writer
            .set_remote_tracking_ref("origin", "main", &first)
            .unwrap();
        let initial = session.status_incremental().unwrap();
        assert_eq!(initial.status.ahead, 0);
        assert_eq!(
            session
                .repository_metadata()
                .unwrap()
                .upstream_target
                .as_deref(),
            Some(first.as_str())
        );

        fs::write(&note, "two\n").unwrap();
        session.add_all().unwrap();
        let second = session.commit("second").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let ahead = session.status_incremental().unwrap();
        assert_eq!(ahead.status.ahead, 1);

        writer
            .set_remote_tracking_ref("origin", "main", &second)
            .unwrap();
        let hot_synced = session.status_incremental().unwrap();
        assert!(hot_synced.telemetry.status_cache_hit);
        assert!(hot_synced.telemetry.persistent_snapshot_saved);
        assert_eq!(hot_synced.status.ahead, 0);
        assert_eq!(hot_synced.status.behind, 0);
        assert!(hot_synced.generation > ahead.generation);
        assert_eq!(
            session
                .repository_metadata()
                .unwrap()
                .upstream_target
                .as_deref(),
            Some(second.as_str())
        );

        session.close().unwrap();
        writer
            .set_remote_tracking_ref("origin", "main", &first)
            .unwrap();
        session.open().unwrap();
        let reopened_ahead = session.status_incremental().unwrap();
        assert!(reopened_ahead.telemetry.persistent_snapshot_hit);
        assert!(reopened_ahead.telemetry.status_cache_hit);
        assert_eq!(reopened_ahead.status.ahead, 1);
        assert_eq!(reopened_ahead.status.behind, 0);
    }

    #[test]
    fn persistent_status_snapshot_rejects_older_classification_schema() {
        let directory = tempfile::tempdir().unwrap();
        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        fs::write(directory.path().join("note.txt"), "one\n").unwrap();
        session.add_all().unwrap();
        session.commit("initial").unwrap();
        assert!(
            session
                .status_incremental()
                .unwrap()
                .telemetry
                .persistent_snapshot_saved
        );
        session.close().unwrap();

        let cache_directory = directory.path().join(".graft/cache/sdk-status");
        let snapshot_path = fs::read_dir(&cache_directory)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .unwrap();
        let mut snapshot =
            serde_json::from_slice::<PersistedStatusSnapshot>(&fs::read(&snapshot_path).unwrap())
                .unwrap();
        snapshot.schema_version = STATUS_SNAPSHOT_SCHEMA_VERSION - 1;
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        for entry in fs::read_dir(&cache_directory)
            .unwrap()
            .filter_map(std::result::Result::ok)
        {
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                fs::remove_file(entry.path()).unwrap();
            }
        }
        fs::write(
            cache_directory.join(format!(
                "classification-v1-{}.json",
                blake3::hash(&bytes).to_hex()
            )),
            bytes,
        )
        .unwrap();

        session.open().unwrap();
        let rebuilt = session.status_incremental().unwrap();
        assert!(!rebuilt.telemetry.persistent_snapshot_hit);
        assert!(rebuilt.telemetry.persistent_snapshot_saved);
    }

    #[test]
    fn metadata_and_remote_projection_do_not_scan_or_expose_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        fs::write(directory.path().join("note.txt"), "one\n").unwrap();
        session.add_all().unwrap();
        let committed = session.commit("initial").unwrap();
        session
            .configure_remote(&RemoteConfigureOptions {
                name: "origin".to_string(),
                url: "https://example.invalid/acme/repo".to_string(),
                bearer_token: Some("never-persist-this-token".to_string()),
                overwrite: false,
                upstream_branch: None,
            })
            .unwrap();

        let metadata = session.repository_metadata().unwrap();
        assert_eq!(
            metadata.current_head.as_deref(),
            committed.pointer("/commit/id").and_then(Value::as_str)
        );
        assert_eq!(metadata.current_branch.as_deref(), Some("main"));
        assert_eq!(metadata.telemetry.paths_examined, 0);
        let remotes = session.list_remotes().unwrap();
        assert_eq!(remotes.telemetry.paths_examined, 0);
        assert_eq!(remotes.remotes.len(), 1);
        assert_eq!(remotes.remotes[0].kind, SafeRemoteKind::Http);
        assert_eq!(remotes.remotes[0].url, "https://example.invalid/acme/repo");
        assert!(!serde_json::to_string(&remotes).unwrap().contains("token"));

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = with_cancellation(&cancellation, || session.repository_metadata()).unwrap_err();
        assert_eq!(error.code(), SdkErrorCode::Cancelled);
        assert_eq!(session.list_remotes().unwrap().telemetry.paths_examined, 0);
    }

    #[test]
    fn revision_path_content_is_bounded_cancellable_and_non_materializing() {
        let directory = tempfile::tempdir().unwrap();
        let note = directory.path().join("note.txt");
        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();

        fs::write(&note, "one\n").unwrap();
        session.add_all().unwrap();
        let root = session.commit("root text").unwrap();
        let root_id = root["commit"]["id"].as_str().unwrap().to_string();
        let root_content = session
            .read_path_content(&ReadPathContentOptions {
                path: PathBuf::from("note.txt"),
                revision: root_id.clone(),
                max_bytes: 1024,
            })
            .unwrap();
        assert_eq!(root_content.revision, root_id);
        assert_eq!(root_content.path, "note.txt");
        assert_eq!(
            root_content.kind,
            Some(graft::repo::RepoTrackedPathKind::TextFile)
        );
        assert!(matches!(
            root_content.content,
            RepoPathContentState::Utf8 { ref content, size: 4, .. } if content == "one\n"
        ));
        let absent = session
            .read_path_content(&ReadPathContentOptions {
                path: PathBuf::from("missing.txt"),
                revision: root_id.clone(),
                max_bytes: 1024,
            })
            .unwrap();
        assert_eq!(absent.kind, None);
        assert!(matches!(absent.content, RepoPathContentState::Absent));

        fs::write(&note, "two\n").unwrap();
        session.add_all().unwrap();
        let updated = session.commit("update text").unwrap();
        let updated_id = updated["commit"]["id"].as_str().unwrap().to_string();
        let before = session
            .read_path_content(&ReadPathContentOptions {
                path: PathBuf::from("note.txt"),
                revision: root_id.clone(),
                max_bytes: 1024,
            })
            .unwrap();
        let after = session
            .read_path_content(&ReadPathContentOptions {
                path: PathBuf::from("note.txt"),
                revision: updated_id.clone(),
                max_bytes: 1024,
            })
            .unwrap();
        assert!(matches!(
            before.content,
            RepoPathContentState::Utf8 { ref content, .. } if content == "one\n"
        ));
        assert!(matches!(
            after.content,
            RepoPathContentState::Utf8 { ref content, .. } if content == "two\n"
        ));

        let bounded = session
            .read_path_content(&ReadPathContentOptions {
                path: PathBuf::from("note.txt"),
                revision: updated_id.clone(),
                max_bytes: 3,
            })
            .unwrap();
        assert!(matches!(
            bounded.content,
            RepoPathContentState::TooLarge { size: 4, .. }
        ));

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let error = with_cancellation(&cancelled, || {
            session.read_path_content(&ReadPathContentOptions {
                path: PathBuf::from("note.txt"),
                revision: root_id.clone(),
                max_bytes: 1024,
            })
        })
        .unwrap_err();
        assert_eq!(error.code(), SdkErrorCode::Cancelled);
        assert!(session.repository_metadata().is_ok());

        let error = session
            .read_path_content(&ReadPathContentOptions {
                path: PathBuf::from("note.txt"),
                revision: updated_id,
                max_bytes: MAX_PATH_CONTENT_BYTES + 1,
            })
            .unwrap_err();
        assert_eq!(error.code(), SdkErrorCode::InvalidArgument);
    }

    #[test]
    fn sqlite_diff_summary_and_rows_are_independently_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("space.eidos");
        let database = rusqlite::Connection::open(&database_path).unwrap();
        database
            .execute_batch("CREATE TABLE records (id INTEGER PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        drop(database);

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        let baseline = session.commit("empty records").unwrap();
        let baseline = baseline["commit"]["id"].as_str().unwrap().to_string();

        let mut database = rusqlite::Connection::open(&database_path).unwrap();
        let transaction = database.transaction().unwrap();
        for index in 0..1_000 {
            transaction
                .execute(
                    "INSERT INTO records (value) VALUES (?1)",
                    [format!("value-{index}")],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        drop(database);
        session.add_all().unwrap();
        let updated = session.commit("insert records").unwrap();
        let updated = updated["commit"]["id"].as_str().unwrap().to_string();
        let base_options = |response| SqliteDiffPathsOptions {
            paths: vec![PathBuf::from("space.eidos")],
            staged: false,
            staged_fallback: false,
            root: None,
            from: Some(baseline.clone()),
            to: Some(updated.clone()),
            response,
            limit: 1,
            after: None,
        };

        let summary = session
            .diff_sqlite_paths(&base_options(SqliteDiffResponse::Summary))
            .unwrap();
        assert_eq!(summary.telemetry.rows_returned, 0);
        assert_eq!(summary.telemetry.response_scope, "streaming_rowid");
        assert_eq!(
            summary.paths[0].diff["files"][0]["summaries"][0]["inserts"],
            1_000
        );

        let first = session
            .diff_sqlite_paths(&base_options(SqliteDiffResponse::Rows {
                table: "records".to_string(),
                limit: 2,
                after: None,
            }))
            .unwrap();
        assert_eq!(first.telemetry.rows_returned, 2);
        assert!(first.telemetry.truncated);
        let file = &first.paths[0].diff["files"][0];
        assert_eq!(file["tables"][0]["changes"].as_array().unwrap().len(), 2);
        let cursor = file["next_cursor"].as_str().unwrap().to_string();

        let second = session
            .diff_sqlite_paths(&base_options(SqliteDiffResponse::Rows {
                table: "records".to_string(),
                limit: 2,
                after: Some(cursor),
            }))
            .unwrap();
        assert_eq!(
            second.paths[0].diff["files"][0]["tables"][0]["changes"][0]["rowid"],
            3
        );

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let error = with_cancellation(&cancelled, || {
            session.diff_sqlite_paths(&base_options(SqliteDiffResponse::Summary))
        })
        .unwrap_err();
        assert_eq!(error.code(), SdkErrorCode::Cancelled);
    }

    #[test]
    fn worktree_sqlite_diff_skips_large_tables_after_nullable_column_append() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("space.eidos");
        let mut database = rusqlite::Connection::open(&database_path).unwrap();
        database
            .execute_batch(
                "CREATE TABLE archive (
                    id TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                 ) STRICT, WITHOUT ROWID;
                 CREATE TABLE notes (
                    id TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                 ) STRICT, WITHOUT ROWID;",
            )
            .unwrap();
        let transaction = database.transaction().unwrap();
        for index in 0..20_000 {
            transaction
                .execute(
                    "INSERT INTO archive (id, value) VALUES (?1, ?2)",
                    [format!("archive-{index:05}"), format!("value-{index}")],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        database
            .execute(
                "INSERT INTO notes (id, value) VALUES ('note-0', 'baseline')",
                [],
            )
            .unwrap();
        drop(database);

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        session.commit("large baseline").unwrap();

        let database = rusqlite::Connection::open(&database_path).unwrap();
        database
            .execute_batch(
                "ALTER TABLE notes ADD COLUMN detail TEXT;
                 INSERT INTO notes (id, value) VALUES ('note-1', 'one');
                 INSERT INTO notes (id, value, detail) VALUES ('note-2', 'two', 'new field');",
            )
            .unwrap();
        drop(database);

        let options = |response| SqliteDiffPathsOptions {
            paths: vec![PathBuf::from("space.eidos")],
            staged: false,
            staged_fallback: false,
            root: None,
            from: None,
            to: None,
            response,
            limit: 1,
            after: None,
        };
        let summary = session
            .diff_sqlite_paths(&options(SqliteDiffResponse::Summary))
            .unwrap();
        let file = &summary.paths[0].diff["files"][0];
        assert_eq!(file["summaries"].as_array().unwrap().len(), 1);
        assert_eq!(file["summaries"][0]["name"], "notes");
        assert_eq!(file["summaries"][0]["inserts"], 2);
        assert_eq!(summary.telemetry.tables_scanned, 1);
        assert_eq!(summary.telemetry.response_scope, "streaming_primary_key");

        let rows = session
            .diff_sqlite_paths(&options(SqliteDiffResponse::Rows {
                table: "notes".to_string(),
                limit: 100,
                after: None,
            }))
            .unwrap();
        assert_eq!(rows.telemetry.tables_scanned, 1);
        assert_eq!(rows.telemetry.rows_returned, 2);
        assert_eq!(rows.telemetry.response_scope, "streaming_primary_key");
        assert_eq!(
            rows.paths[0].diff["files"][0]["tables"][0]["changes"][1]["values"][2],
            "new field"
        );

        session
            .stage_paths(&StagePathsOptions {
                paths: vec![PathBuf::from("space.eidos")],
                expected_head: None,
                force: false,
            })
            .unwrap();
        session.commit("small table change").unwrap();
        let history = session.history_summaries(1, None).unwrap();
        assert_eq!(history.commits[0].changed_tables, 1);
        assert_eq!(history.commits[0].tables[0].name, "notes");
        assert_eq!(history.commits[0].tables[0].inserts, 2);

        let database = rusqlite::Connection::open(&database_path).unwrap();
        database
            .execute_batch(
                "DELETE FROM archive WHERE id = 'archive-00001';
                 UPDATE archive SET value = 'changed' WHERE id = 'archive-10000';
                 INSERT INTO archive (id, value) VALUES ('archive-99999', 'new');",
            )
            .unwrap();
        drop(database);

        // Requesting one table first must not require a whole-file worktree probe.
        let rows = session
            .diff_sqlite_paths(&options(SqliteDiffResponse::Rows {
                table: "archive".to_string(),
                limit: 100,
                after: None,
            }))
            .unwrap();
        assert_eq!(rows.telemetry.rows_returned, 3);
        assert!(rows.telemetry.rows_scanned < 2_000);

        let summary = session
            .diff_sqlite_paths(&options(SqliteDiffResponse::Summary))
            .unwrap();
        let file = &summary.paths[0].diff["files"][0];
        assert_eq!(file["summaries"].as_array().unwrap().len(), 1);
        assert_eq!(file["summaries"][0]["name"], "archive");
        assert_eq!(file["summaries"][0]["inserts"], 1);
        assert_eq!(file["summaries"][0]["deletes"], 1);
        assert_eq!(file["summaries"][0]["updates"], 1);
        assert!(summary.telemetry.rows_scanned < 2_000);

        session
            .stage_paths(&StagePathsOptions {
                paths: vec![PathBuf::from("space.eidos")],
                expected_head: None,
                force: false,
            })
            .unwrap();
        session.commit("large table sparse change").unwrap();
        let history = session.history_summaries(1, None).unwrap();
        assert_eq!(history.commits[0].changed_tables, 1);
        assert_eq!(history.commits[0].tables[0].name, "archive");
        assert_eq!(history.commits[0].tables[0].inserts, 1);
        assert_eq!(history.commits[0].tables[0].deletes, 1);
        assert_eq!(history.commits[0].tables[0].updates, 1);
    }

    #[test]
    fn concurrent_path_type_churn_never_poison_session() {
        let directory = tempfile::tempdir().unwrap();
        let shape = directory.path().join("shape");
        fs::write(&shape, "tracked\n").unwrap();
        let session = Arc::new(RepositorySession::new(directory.path()));
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        session.commit("track shape").unwrap();
        session.status_incremental().unwrap();

        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let churn_running = running.clone();
        let churn_shape = shape.clone();
        let churn = thread::spawn(move || {
            let renamed = churn_shape.with_file_name("shape-renamed");
            while churn_running.load(Ordering::Acquire) {
                let _ = fs::remove_file(&churn_shape);
                let _ = fs::create_dir(&churn_shape);
                let _ = fs::write(churn_shape.join("nested.txt"), "nested\n");
                let _ = fs::remove_file(churn_shape.join("nested.txt"));
                let _ = fs::remove_dir(&churn_shape);
                let _ = fs::write(&churn_shape, "replacement\n");
                let _ = fs::rename(&churn_shape, &renamed);
                let _ = fs::rename(&renamed, &churn_shape);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::symlink;
                    let _ = fs::remove_file(&churn_shape);
                    let _ = symlink("missing-shape-target", &churn_shape);
                    let _ = fs::remove_file(&churn_shape);
                }
            }
        });

        for _ in 0..20 {
            match session.status_incremental() {
                Ok(_) => {}
                Err(error) => assert_eq!(error.code(), SdkErrorCode::RepositoryStale, "{error}"),
            }
            match session.diff_paths(&DiffPathsOptions {
                paths: vec![PathBuf::from("shape")],
                rows: false,
                root: None,
                from: None,
                to: None,
                table: None,
                limit: 1,
                after: None,
            }) {
                Ok(_) => {}
                Err(error) => assert_eq!(error.code(), SdkErrorCode::RepositoryStale, "{error}"),
            }
            match session
                .is_ignored_paths(&IgnoredPathsOptions { paths: vec![PathBuf::from("shape")] })
            {
                Ok(_) => {}
                Err(error) => assert_eq!(error.code(), SdkErrorCode::RepositoryStale, "{error}"),
            }
            match session.inventory(&InventoryOptions {
                kind: InventoryKind::Untracked,
                limit: 10,
                after: None,
            }) {
                Ok(_) => {}
                Err(error) => assert_eq!(error.code(), SdkErrorCode::RepositoryStale, "{error}"),
            }
        }
        running.store(false, Ordering::Release);
        churn.join().unwrap();
        if shape.is_dir() {
            fs::remove_dir_all(&shape).unwrap();
        } else {
            let _ = fs::remove_file(&shape);
        }
        fs::write(&shape, "stable\n").unwrap();
        assert!(session.status_incremental().is_ok());
        assert!(session.repository_metadata().is_ok());
    }

    #[test]
    fn batch_ignore_queries_preserve_tracked_directories_and_cache_inventory() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("nested");
        let generated = nested.join("generated");
        fs::create_dir_all(&generated).unwrap();
        fs::write(generated.join("cache.txt"), "tracked\n").unwrap();
        fs::write(nested.join("note.txt"), "tracked\n").unwrap();

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        session.commit("track nested files").unwrap();
        fs::write(nested.join(".gitignore"), "generated/\n").unwrap();
        session.add_all().unwrap();
        session.commit("ignore generated files").unwrap();

        let queried = session
            .is_ignored_paths(&IgnoredPathsOptions {
                paths: [
                    "nested",
                    "nested/generated",
                    "nested/generated/cache.txt",
                    "nested/note.txt",
                ]
                .map(PathBuf::from)
                .to_vec(),
            })
            .unwrap();
        assert!(!queried.telemetry.index_cache_hit);
        assert_eq!(queried.paths.len(), 4);
        assert!(queried.paths[0].is_directory);
        assert!(queried.paths[0].has_tracked_descendants);
        assert!(!queried.paths[0].is_ignored);
        assert!(queried.paths[1].is_directory);
        assert!(queried.paths[1].has_tracked_descendants);
        assert!(queried.paths[1].is_ignored);
        assert!(!queried.paths[1].is_tracked);
        assert!(!queried.paths[2].is_directory);
        assert!(!queried.paths[2].has_tracked_descendants);
        assert!(queried.paths[2].is_ignored);
        assert!(queried.paths[2].is_tracked);
        assert!(!queried.paths[3].is_ignored);
        assert!(queried.paths[3].is_tracked);

        let first_inventory = session
            .inventory(&InventoryOptions {
                kind: InventoryKind::TrackedIgnored,
                limit: 1,
                after: None,
            })
            .unwrap();
        assert!(!first_inventory.telemetry.inventory_cache_hit);
        assert_eq!(first_inventory.total_matching, 1);
        assert_eq!(first_inventory.items[0].path, "nested/generated/cache.txt");
        let hot_inventory = session
            .inventory(&InventoryOptions {
                kind: InventoryKind::TrackedIgnored,
                limit: 1,
                after: None,
            })
            .unwrap();
        assert!(hot_inventory.telemetry.inventory_cache_hit);
        assert!(hot_inventory.telemetry.index_cache_hit);
        assert!(hot_inventory.telemetry.ignore_matcher_cache_hit);
        assert_eq!(hot_inventory.telemetry.paths_examined, 0);

        fs::write(nested.join(".gitignore"), "other/\n").unwrap();
        let refreshed_inventory = session
            .inventory(&InventoryOptions {
                kind: InventoryKind::TrackedIgnored,
                limit: 1,
                after: None,
            })
            .unwrap();
        assert!(!refreshed_inventory.telemetry.inventory_cache_hit);
        assert!(!refreshed_inventory.telemetry.ignore_matcher_cache_hit);
        assert_eq!(refreshed_inventory.total_matching, 0);

        let too_many = session
            .is_ignored_paths(&IgnoredPathsOptions {
                paths: (0..=MAX_IGNORE_QUERY_PATHS)
                    .map(|index| PathBuf::from(format!("path-{index}")))
                    .collect(),
            })
            .unwrap_err();
        assert_eq!(too_many.code(), SdkErrorCode::InvalidArgument);

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let cancellation_error = with_cancellation(&cancelled, || {
            session.is_ignored_paths(&IgnoredPathsOptions { paths: vec![PathBuf::from("nested")] })
        })
        .unwrap_err();
        assert_eq!(cancellation_error.code(), SdkErrorCode::Cancelled);
        assert_eq!(
            session
                .is_ignored_paths(&IgnoredPathsOptions { paths: vec![PathBuf::from("nested")] })
                .unwrap()
                .paths
                .len(),
            1
        );
    }

    #[test]
    fn untrack_paths_keeps_ignored_files_and_honors_cas_limits_and_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let generated_directory = directory.path().join("generated");
        let generated_file = generated_directory.join("cache.txt");
        fs::create_dir_all(&generated_directory).unwrap();
        fs::write(&generated_file, "keep me\n").unwrap();

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        session.commit("track generated file").unwrap();
        fs::write(directory.path().join(".gitignore"), "generated/\n").unwrap();
        session.add_all().unwrap();
        session.commit("ignore generated files").unwrap();
        let expected_head = session.status().unwrap()["current_head"]
            .as_str()
            .unwrap()
            .to_string();

        let directory_error = session
            .untrack_paths(&UntrackPathsOptions {
                paths: vec![PathBuf::from("generated")],
                expected_head: Some(expected_head.clone()),
            })
            .unwrap_err();
        assert_eq!(directory_error.code(), SdkErrorCode::InvalidArgument);

        let too_many_error = session
            .untrack_paths(&UntrackPathsOptions {
                paths: (0..=MAX_BATCH_MUTATION_PATHS)
                    .map(|index| PathBuf::from(format!("path-{index}")))
                    .collect(),
                expected_head: None,
            })
            .unwrap_err();
        assert_eq!(too_many_error.code(), SdkErrorCode::InvalidArgument);

        let cas_error = session
            .untrack_paths(&UntrackPathsOptions {
                paths: vec![PathBuf::from("generated/cache.txt")],
                expected_head: Some("deadbeef".to_string()),
            })
            .unwrap_err();
        assert_eq!(cas_error.code(), SdkErrorCode::InvalidArgument);

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let cancellation_error = with_cancellation(&cancelled, || {
            session.untrack_paths(&UntrackPathsOptions {
                paths: vec![PathBuf::from("generated/cache.txt")],
                expected_head: Some(expected_head.clone()),
            })
        })
        .unwrap_err();
        assert_eq!(cancellation_error.code(), SdkErrorCode::Cancelled);

        let untracked = session
            .untrack_paths(&UntrackPathsOptions {
                paths: vec![PathBuf::from("generated/cache.txt")],
                expected_head: Some(expected_head.clone()),
            })
            .unwrap();
        assert_eq!(untracked.paths.len(), 1);
        assert!(!untracked.materializes_worktree);
        assert_eq!(fs::read_to_string(&generated_file).unwrap(), "keep me\n");

        session.add_all().unwrap();
        let ignored = session
            .is_ignored_path(Path::new("generated/cache.txt"))
            .unwrap();
        assert!(ignored.is_ignored);
        assert!(!ignored.is_tracked);
        assert_eq!(
            session.status().unwrap()["current_head"],
            json!(expected_head)
        );
    }

    #[test]
    fn second_repository_session_reports_busy_until_first_closes() {
        let directory = tempfile::tempdir().unwrap();
        let first = RepositorySession::new(directory.path());
        first.open().unwrap();
        first.init().unwrap();

        let second = RepositorySession::new(directory.path());
        let error = second.open().unwrap_err();
        assert_eq!(error.code(), SdkErrorCode::RepositoryBusy);

        first.close().unwrap();
        second.open().unwrap();
        second.close().unwrap();
    }

    #[test]
    fn same_session_serializes_concurrent_calls() {
        let directory = tempfile::tempdir().unwrap();
        let session = Arc::new(RepositorySession::new(directory.path()));
        session.open().unwrap();
        session.init().unwrap();
        fs::write(directory.path().join("note.txt"), "one\n").unwrap();
        session.add_all().unwrap();
        session.commit("initial").unwrap();

        let barrier = Arc::new(Barrier::new(9));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let session = session.clone();
            let barrier = barrier.clone();
            threads.push(thread::spawn(move || {
                barrier.wait();
                for _ in 0..10 {
                    session.status().unwrap();
                    session.diff(&DiffOptions::default()).unwrap();
                }
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
    }

    #[test]
    fn different_repositories_run_on_independent_session_locks() {
        let first_directory = tempfile::tempdir().unwrap();
        let second_directory = tempfile::tempdir().unwrap();
        let first = Arc::new(RepositorySession::new(first_directory.path()));
        let second = Arc::new(RepositorySession::new(second_directory.path()));
        first.open().unwrap();
        second.open().unwrap();
        first.init().unwrap();
        second.init().unwrap();

        let barrier = Arc::new(Barrier::new(3));
        let handles = [first, second].map(|session| {
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                for _ in 0..20 {
                    session.status().unwrap();
                    session.diff(&DiffOptions::default()).unwrap();
                }
            })
        });
        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn close_rejects_queued_work_and_waits_for_in_flight_holder() {
        let directory = tempfile::tempdir().unwrap();
        let session = Arc::new(RepositorySession::new(directory.path()));
        session.open().unwrap();
        session.init().unwrap();

        let in_flight = session.state.lock();
        let (closed, received) = mpsc::channel();
        let closing_session = session.clone();
        let close = thread::spawn(move || {
            closing_session.close().unwrap();
            closed.send(()).unwrap();
        });
        while session.lifecycle() != SessionLifecycle::Closing {
            thread::yield_now();
        }

        assert_eq!(
            session.status().unwrap_err().code(),
            SdkErrorCode::SessionClosing
        );
        assert!(received.recv_timeout(Duration::from_millis(20)).is_err());
        drop(in_flight);
        received.recv_timeout(Duration::from_secs(1)).unwrap();
        close.join().unwrap();
        assert_eq!(session.lifecycle(), SessionLifecycle::Closed);
    }

    #[test]
    fn open_application_database_handle_does_not_block_non_materializing_calls() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("app.eidos");
        let database = Connection::open(&database_path).unwrap();
        database
            .execute("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)", [])
            .unwrap();
        database
            .execute("INSERT INTO items (name) VALUES ('one')", [])
            .unwrap();

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        let committed = session.commit("initial").unwrap();
        assert!(committed.get("materialized").is_none());
        database
            .execute("INSERT INTO items (name) VALUES ('two')", [])
            .unwrap();
        let observer = Connection::open(&database_path).unwrap();
        assert_eq!(
            observer
                .query_row("SELECT COUNT(*) FROM items", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
        drop(observer);
        assert_eq!(session.status().unwrap()["dirty"], json!(true));
        session.add_all().unwrap();
        session.commit("second database snapshot").unwrap();
        assert_eq!(session.status().unwrap()["dirty"], json!(false));
        session
            .diff(&DiffOptions { rows: true, ..DiffOptions::default() })
            .unwrap();

        drop(database);
        fs::write(directory.path().join("note.txt"), "materialized later\n").unwrap();
        session.add_all().unwrap();
        session.commit("second").unwrap();
        session.close().unwrap();

        let reopened = Connection::open(&database_path).unwrap();
        assert_eq!(
            reopened
                .query_row("SELECT COUNT(*) FROM items", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn reopened_session_keeps_physical_sqlite_authoritative_after_each_checkpoint() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("app.eidos");
        let database = Connection::open(&database_path).unwrap();
        database
            .execute(
                "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
                [],
            )
            .unwrap();
        database
            .execute("INSERT INTO items (name) VALUES ('one')", [])
            .unwrap();
        drop(database);

        let initial = RepositorySession::new(directory.path());
        initial.open().unwrap();
        initial.init().unwrap();
        initial.add_all().unwrap();
        initial.commit("initial").unwrap();
        initial.close().unwrap();

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        for (name, message) in [("two", "second"), ("three", "third")] {
            let database = Connection::open(&database_path).unwrap();
            database
                .execute("UPDATE items SET name = ?1 WHERE id = 1", [name])
                .unwrap();
            drop(database);

            assert!(session.status_incremental().unwrap().status.dirty);
            session
                .stage_paths(&StagePathsOptions {
                    paths: vec![PathBuf::from("app.eidos")],
                    expected_head: None,
                    force: false,
                })
                .unwrap();
            session.commit(message).unwrap();
            assert!(!session.status_incremental().unwrap().status.dirty);
        }
        session.close().unwrap();

        let database = Connection::open(&database_path).unwrap();
        assert_eq!(
            database
                .query_row("SELECT name FROM items WHERE id = 1", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "three"
        );
    }

    #[test]
    fn merge_policy_is_typed_cas_bound_to_plans_and_frozen_during_merge() {
        let directory = tempfile::tempdir().unwrap();
        let note = directory.path().join("note.txt");
        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        fs::write(&note, "base\n").unwrap();
        session.add_all().unwrap();
        session.commit("base").unwrap();

        let initial = session.get_merge_policy().unwrap();
        assert_eq!(initial.policy.version, graft::repo::MERGE_POLICY_VERSION);
        assert!(!initial.active_merge);
        let invalid = session.validate_merge_policy(&MergePolicyDocument {
            version: 999,
            config: MergeConfig::default(),
        });
        assert!(!invalid.valid);
        assert_eq!(invalid.errors[0].key, "version");

        let enabled = MergeConfig {
            same_row_merge: true,
            ..Default::default()
        };
        let enabled = session
            .set_merge_policy(&SetMergePolicyOptions {
                policy: MergePolicyDocument { version: 1, config: enabled },
                expected_policy_token: initial.policy_token.clone(),
            })
            .unwrap();
        assert_ne!(enabled.policy_token, initial.policy_token);
        let stale = session
            .set_merge_policy(&SetMergePolicyOptions {
                policy: MergePolicyDocument {
                    version: 1,
                    config: MergeConfig::default(),
                },
                expected_policy_token: initial.policy_token,
            })
            .unwrap_err();
        assert_eq!(stale.code(), SdkErrorCode::RepositoryStale);

        session.close().unwrap();
        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_new_branch("hosted", None).unwrap();
        fs::write(&note, "hosted\n").unwrap();
        repo.stage_artifact_path(&note).unwrap();
        let hosted = repo.commit_staged("hosted").unwrap();
        repo.switch_branch("main").unwrap();
        fs::write(&note, "local\n").unwrap();
        repo.stage_artifact_path(&note).unwrap();
        let local = repo.commit_staged("local").unwrap();
        drop(repo);
        session.open().unwrap();

        let stale_plan = session
            .plan_merge(&PlanMergeOptions {
                revision: hosted.id.clone(),
                expected_head: Some(local.id.clone()),
            })
            .unwrap();
        assert_eq!(stale_plan.policy_token, enabled.policy_token);
        let changed = session
            .set_merge_policy(&SetMergePolicyOptions {
                policy: MergePolicyDocument {
                    version: 1,
                    config: MergeConfig::default(),
                },
                expected_policy_token: enabled.policy_token,
            })
            .unwrap();
        let error = session
            .apply_merge(&ApplyMergeOptions {
                revision: hosted.id.clone(),
                expected_head: Some(local.id.clone()),
                plan_token: stale_plan.plan_token,
            })
            .unwrap_err();
        assert_eq!(error.code(), SdkErrorCode::RepositoryStale);

        let plan = session
            .plan_merge(&PlanMergeOptions {
                revision: hosted.id.clone(),
                expected_head: Some(local.id.clone()),
            })
            .unwrap();
        assert_eq!(plan.policy_token, changed.policy_token);
        let applied = session
            .apply_merge(&ApplyMergeOptions {
                revision: hosted.id,
                expected_head: Some(local.id),
                plan_token: plan.plan_token,
            })
            .unwrap();
        let MergeStatus::Merging {
            state_token,
            policy_token,
            policy_version,
            ..
        } = applied.merge
        else {
            panic!("expected active merge");
        };
        assert_eq!(policy_token, changed.policy_token);
        assert_eq!(policy_version, 1);
        let frozen = session.get_merge_policy().unwrap();
        assert!(frozen.active_merge);
        assert_eq!(frozen.policy_token, policy_token);
        let error = session
            .set_merge_policy(&SetMergePolicyOptions {
                policy: frozen.policy,
                expected_policy_token: policy_token,
            })
            .unwrap_err();
        assert_eq!(error.code(), SdkErrorCode::RepositoryStale);
        session
            .abort_merge(&AbortMergeOptions { expected_state_token: state_token })
            .unwrap();
        session.close().unwrap();
    }

    #[test]
    fn directory_session_auto_materializes_every_conflict_free_sqlite_path() {
        let directory = tempfile::tempdir().unwrap();
        let paths = [
            directory.path().join("one.eidos"),
            directory.path().join("two.db"),
        ];
        let rewrite = |path: &Path, first: &str, second: &str| {
            for candidate in [
                path.to_path_buf(),
                PathBuf::from(format!("{}-wal", path.display())),
                PathBuf::from(format!("{}-shm", path.display())),
            ] {
                let _ = fs::remove_file(candidate);
            }
            let database = Connection::open(path).unwrap();
            database
                .execute_batch(&format!(
                    "CREATE TABLE docs (id INTEGER PRIMARY KEY, value TEXT NOT NULL);\
                     INSERT INTO docs VALUES (1, '{first}'), (2, '{second}');"
                ))
                .unwrap();
        };
        for path in &paths {
            rewrite(path, "base-one", "base-two");
        }

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        session.commit("base").unwrap();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_new_branch("hosted", None).unwrap();
        drop(repo);
        for path in &paths {
            let database = Connection::open(path).unwrap();
            database
                .execute("UPDATE docs SET value = 'hosted-two' WHERE id = 2", [])
                .unwrap();
        }
        session.open().unwrap();
        session.add_all().unwrap();
        let hosted = session.commit("hosted rows").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_branch("main").unwrap();
        drop(repo);
        for path in &paths {
            rewrite(path, "local-one", "base-two");
        }
        session.open().unwrap();
        session.add_all().unwrap();
        let local = session.commit("local rows").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let plan = session
            .plan_merge(&PlanMergeOptions {
                revision: hosted.clone(),
                expected_head: Some(local.clone()),
            })
            .unwrap();
        assert_eq!(plan.conflicted_paths, vec!["one.eidos", "two.db"]);
        let applied = session
            .apply_merge(&ApplyMergeOptions {
                revision: hosted,
                expected_head: Some(local),
                plan_token: plan.plan_token,
            })
            .unwrap();
        let MergeStatus::Merging { unmerged_count, state_token, .. } = applied.merge else {
            panic!("expected active merge");
        };
        assert_eq!(unmerged_count, 0);
        for path in &paths {
            let database = Connection::open(path).unwrap();
            let rows = database
                .prepare("SELECT value FROM docs ORDER BY id")
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(rows, vec!["local-one", "hosted-two"]);
        }
        session
            .abort_merge(&AbortMergeOptions { expected_state_token: state_token })
            .unwrap();
        session.close().unwrap();
    }

    #[test]
    fn explicit_same_row_policy_combines_fields_resolves_cells_and_requires_recompute_validation() {
        let directory = tempfile::tempdir().unwrap();
        let disjoint = directory.path().join("disjoint.db");
        let equal = directory.path().join("equal.db");
        let cell = directory.path().join("cell.db");
        let managed = directory.path().join("managed.db");
        let derived = directory.path().join("derived.db");
        for path in [&disjoint, &equal, &cell] {
            let database = Connection::open(path).unwrap();
            database
                .execute_batch(
                    "CREATE TABLE records (id INTEGER PRIMARY KEY, a TEXT NOT NULL, b TEXT NOT NULL);\
                     INSERT INTO records VALUES (1, 'base-a', 'base-b');",
                )
                .unwrap();
        }
        {
            let database = Connection::open(&managed).unwrap();
            database
                .execute_batch(
                    "CREATE TABLE records (id INTEGER PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL);\
                     INSERT INTO records VALUES (1, 'base', 0);",
                )
                .unwrap();
        }
        {
            let database = Connection::open(&derived).unwrap();
            database
                .execute_batch(
                    "CREATE TABLE records (id INTEGER PRIMARY KEY, left_value TEXT NOT NULL, right_value TEXT NOT NULL, derived TEXT NOT NULL);\
                     INSERT INTO records VALUES (1, 'base-left', 'base-right', 'base-derived');",
                )
                .unwrap();
        }

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        session.commit("base").unwrap();
        let initial_policy = session.get_merge_policy().unwrap();
        let mut policy = MergeConfig {
            same_row_merge: true,
            ..Default::default()
        };
        policy.column_resolvers.insert(
            "records".to_string(),
            BTreeMap::from([
                (
                    "updated_at".to_string(),
                    ManagedColumnResolver::MaxTimestamp,
                ),
                ("derived".to_string(), ManagedColumnResolver::Recompute),
            ]),
        );
        session
            .set_merge_policy(&SetMergePolicyOptions {
                policy: MergePolicyDocument { version: 1, config: policy },
                expected_policy_token: initial_policy.policy_token,
            })
            .unwrap();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_new_branch("hosted", None).unwrap();
        drop(repo);
        Connection::open(&disjoint)
            .unwrap()
            .execute("UPDATE records SET b = 'hosted-b' WHERE id = 1", [])
            .unwrap();
        Connection::open(&cell)
            .unwrap()
            .execute("UPDATE records SET a = 'hosted-a' WHERE id = 1", [])
            .unwrap();
        Connection::open(&equal)
            .unwrap()
            .execute("UPDATE records SET a = 'shared-a' WHERE id = 1", [])
            .unwrap();
        Connection::open(&managed)
            .unwrap()
            .execute(
                "UPDATE records SET value = 'hosted', updated_at = 20 WHERE id = 1",
                [],
            )
            .unwrap();
        Connection::open(&derived)
            .unwrap()
            .execute(
                "UPDATE records SET right_value = 'hosted-right' WHERE id = 1",
                [],
            )
            .unwrap();
        session.open().unwrap();
        session.add_all().unwrap();
        let hosted = session.commit("hosted").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_branch("main").unwrap();
        drop(repo);
        Connection::open(&disjoint)
            .unwrap()
            .execute(
                "UPDATE records SET a = 'local-a', b = 'base-b' WHERE id = 1",
                [],
            )
            .unwrap();
        Connection::open(&cell)
            .unwrap()
            .execute(
                "UPDATE records SET a = 'local-a', b = 'base-b' WHERE id = 1",
                [],
            )
            .unwrap();
        Connection::open(&equal)
            .unwrap()
            .execute("UPDATE records SET a = 'shared-a' WHERE id = 1", [])
            .unwrap();
        Connection::open(&managed)
            .unwrap()
            .execute(
                "UPDATE records SET value = 'base', updated_at = 10 WHERE id = 1",
                [],
            )
            .unwrap();
        Connection::open(&derived)
            .unwrap()
            .execute(
                "UPDATE records SET left_value = 'local-left', right_value = 'base-right', derived = 'base-derived' WHERE id = 1",
                [],
            )
            .unwrap();
        session.open().unwrap();
        session.add_all().unwrap();
        let local = session.commit("local").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let plan = session
            .plan_merge(&PlanMergeOptions {
                revision: hosted.clone(),
                expected_head: Some(local.clone()),
            })
            .unwrap();
        let applied = session
            .apply_merge(&ApplyMergeOptions {
                revision: hosted,
                expected_head: Some(local),
                plan_token: plan.plan_token,
            })
            .unwrap();
        let MergeStatus::Merging { unmerged_count, state_token, .. } = applied.merge else {
            panic!("expected active merge");
        };
        assert_eq!(unmerged_count, 2);

        assert_eq!(
            Connection::open(&disjoint)
                .unwrap()
                .query_row("SELECT a || ':' || b FROM records", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "local-a:hosted-b"
        );
        assert_eq!(
            Connection::open(&managed)
                .unwrap()
                .query_row(
                    "SELECT value || ':' || updated_at FROM records",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "hosted:20"
        );
        assert_eq!(
            Connection::open(&equal)
                .unwrap()
                .query_row("SELECT a FROM records", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "shared-a"
        );
        assert_eq!(
            Connection::open(&derived)
                .unwrap()
                .query_row(
                    "SELECT left_value || ':' || right_value || ':' || derived FROM records",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "local-left:hosted-right:base-derived"
        );

        let cell_conflicts = session
            .list_merge_conflicts(&ListMergeConflictsOptions {
                path: PathBuf::from("cell.db"),
                limit: 10,
                after: None,
                expected_state_token: state_token.clone(),
            })
            .unwrap();
        assert_eq!(cell_conflicts.items.len(), 1);
        assert_eq!(cell_conflicts.items[0]["reason"], "cell_conflict");
        assert_eq!(cell_conflicts.items[0]["cells"][0]["column"], "a");
        assert_eq!(cell_conflicts.items[0]["cells"][0]["base"], "base-a");
        assert_eq!(cell_conflicts.items[0]["cells"][0]["ours"], "local-a");
        assert_eq!(cell_conflicts.items[0]["cells"][0]["theirs"], "hosted-a");
        let recompute = session
            .list_merge_conflicts(&ListMergeConflictsOptions {
                path: PathBuf::from("derived.db"),
                limit: 10,
                after: None,
                expected_state_token: state_token.clone(),
            })
            .unwrap();
        assert_eq!(recompute.items.len(), 1);
        assert_eq!(recompute.items[0]["kind"], "validation");
        assert_eq!(recompute.items[0]["reason"], "recompute_required");
        assert_eq!(recompute.items[0]["columns"], json!(["derived"]));
        assert_eq!(
            recompute.items[0]["recommended_action"],
            "stage_worktree_result"
        );

        let stale = session
            .resolve_merge_cell(&ResolveMergeCellOptions {
                path: PathBuf::from("cell.db"),
                table: "records".to_string(),
                identity: json!(1),
                column: "a".to_string(),
                result: MergePathResult::Ours,
                expected_state_token: "stale".to_string(),
            })
            .unwrap_err();
        assert_eq!(stale.code(), SdkErrorCode::RepositoryStale);
        let resolved = session
            .resolve_merge_cell(&ResolveMergeCellOptions {
                path: PathBuf::from("cell.db"),
                table: "records".to_string(),
                identity: json!(1),
                column: "a".to_string(),
                result: MergePathResult::Ours,
                expected_state_token: state_token,
            })
            .unwrap();
        let MergeStatus::Merging {
            unmerged_count, state_token: cell_token, ..
        } = resolved.merge
        else {
            panic!("expected recompute validation to remain");
        };
        assert_eq!(unmerged_count, 1);
        assert_eq!(
            Connection::open(&cell)
                .unwrap()
                .query_row("SELECT a FROM records", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "local-a"
        );

        session.close().unwrap();
        session.open().unwrap();
        let MergeStatus::Merging { state_token: reopened, .. } =
            session.get_merge_status().unwrap()
        else {
            panic!("expected durable cell resolution");
        };
        assert_eq!(reopened, cell_token);
        let repeated = session
            .resolve_merge_cell(&ResolveMergeCellOptions {
                path: PathBuf::from("cell.db"),
                table: "records".to_string(),
                identity: json!(1),
                column: "a".to_string(),
                result: MergePathResult::Ours,
                expected_state_token: reopened,
            })
            .unwrap();
        let MergeStatus::Merging { state_token: repeated_token, .. } = repeated.merge else {
            panic!("expected active merge");
        };
        assert_eq!(repeated_token, cell_token);

        Connection::open(&derived)
            .unwrap()
            .execute(
                "UPDATE records SET derived = left_value || '|' || right_value WHERE id = 1",
                [],
            )
            .unwrap();
        let staged = session
            .stage_merge_sqlite_result(&StageMergeSqliteResultOptions {
                path: PathBuf::from("derived.db"),
                expected_state_token: repeated_token,
            })
            .unwrap();
        let MergeStatus::Merging {
            unmerged_count, state_token: final_token, ..
        } = staged.merge
        else {
            panic!("expected active fully resolved merge");
        };
        assert_eq!(unmerged_count, 0);
        assert_eq!(
            Connection::open(&derived)
                .unwrap()
                .query_row("SELECT derived FROM records", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "local-left|hosted-right"
        );

        session
            .abort_merge(&AbortMergeOptions { expected_state_token: final_token })
            .unwrap();
        assert_eq!(
            Connection::open(&disjoint)
                .unwrap()
                .query_row("SELECT a || ':' || b FROM records", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "local-a:base-b"
        );
        assert_eq!(
            Connection::open(&equal)
                .unwrap()
                .query_row("SELECT a FROM records", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "shared-a"
        );
        session.close().unwrap();
    }

    #[test]
    fn semantic_key_nocase_reports_insert_update_and_update_update_collisions() {
        let directory = tempfile::tempdir().unwrap();
        let insert_update = directory.path().join("insert-update.db");
        let update_update = directory.path().join("update-update.db");
        for path in [&insert_update, &update_update] {
            let database = Connection::open(path).unwrap();
            database
                .execute_batch(
                    "CREATE TABLE docs (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, value TEXT NOT NULL);\
                     INSERT INTO docs VALUES (1, 'alpha', 'one'), (2, 'beta', 'two');",
                )
                .unwrap();
        }

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        session.commit("base").unwrap();
        let initial = session.get_merge_policy().unwrap();
        let mut policy = MergeConfig::default();
        policy
            .semantic_keys
            .insert("docs".to_string(), vec!["slug".to_string()]);
        policy.semantic_key_collations.insert(
            "docs".to_string(),
            BTreeMap::from([("slug".to_string(), SemanticKeyCollation::NoCase)]),
        );
        session
            .set_merge_policy(&SetMergePolicyOptions {
                policy: MergePolicyDocument { version: 1, config: policy },
                expected_policy_token: initial.policy_token,
            })
            .unwrap();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_new_branch("hosted", None).unwrap();
        drop(repo);
        Connection::open(&insert_update)
            .unwrap()
            .execute(
                "UPDATE docs SET slug = 'GAMMA', value = 'hosted-update' WHERE id = 1",
                [],
            )
            .unwrap();
        Connection::open(&update_update)
            .unwrap()
            .execute(
                "UPDATE docs SET slug = 'SHARED', value = 'hosted-update' WHERE id = 1",
                [],
            )
            .unwrap();
        session.open().unwrap();
        session.add_all().unwrap();
        let hosted = session.commit("hosted").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_branch("main").unwrap();
        drop(repo);
        {
            let database = Connection::open(&insert_update).unwrap();
            database
                .execute_batch(
                    "UPDATE docs SET slug = 'alpha', value = 'one' WHERE id = 1;\
                     INSERT INTO docs VALUES (3, 'gamma', 'local-insert');",
                )
                .unwrap();
        }
        {
            let database = Connection::open(&update_update).unwrap();
            database
                .execute_batch(
                    "UPDATE docs SET slug = 'alpha', value = 'one' WHERE id = 1;\
                     UPDATE docs SET slug = 'shared', value = 'local-update' WHERE id = 2;",
                )
                .unwrap();
        }
        session.open().unwrap();
        session.add_all().unwrap();
        let local = session.commit("local").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let plan = session
            .plan_merge(&PlanMergeOptions {
                revision: hosted.clone(),
                expected_head: Some(local.clone()),
            })
            .unwrap();
        let applied = session
            .apply_merge(&ApplyMergeOptions {
                revision: hosted,
                expected_head: Some(local),
                plan_token: plan.plan_token,
            })
            .unwrap();
        let MergeStatus::Merging { state_token, .. } = applied.merge else {
            panic!("expected semantic conflicts");
        };

        for (path, expected_ours, expected_theirs, expected_ops) in [
            ("insert-update.db", 3, 1, ("insert", "update")),
            ("update-update.db", 2, 1, ("update", "update")),
        ] {
            let conflicts = session
                .list_merge_conflicts(&ListMergeConflictsOptions {
                    path: PathBuf::from(path),
                    limit: 10,
                    after: None,
                    expected_state_token: state_token.clone(),
                })
                .unwrap();
            assert_eq!(conflicts.items.len(), 1);
            let conflict = &conflicts.items[0];
            assert_eq!(conflict["reason"], "semantic_key_conflict");
            assert_eq!(conflict["ours_rowid"], expected_ours);
            assert_eq!(conflict["theirs_rowid"], expected_theirs);
            assert_eq!(conflict["semantic_key_collations"], json!(["nocase"]));
            assert!(
                conflict["semantic_key"][0]
                    .as_str()
                    .unwrap()
                    .eq_ignore_ascii_case(if path == "insert-update.db" {
                        "t:gamma"
                    } else {
                        "t:shared"
                    })
            );
            assert_eq!(conflict["ours_op"], expected_ops.0);
            assert_eq!(conflict["theirs_op"], expected_ops.1);
        }

        session
            .abort_merge(&AbortMergeOptions { expected_state_token: state_token })
            .unwrap();
        session.close().unwrap();
    }

    #[test]
    fn invalid_sqlite_auto_merge_candidate_preserves_prior_merge_state() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("constraints.db");
        {
            let database = Connection::open(&database_path).unwrap();
            database
                .execute_batch(
                    "PRAGMA foreign_keys = ON;\
                     CREATE TABLE parents (id INTEGER PRIMARY KEY);\
                     CREATE TABLE children (id INTEGER PRIMARY KEY, parent_id INTEGER NOT NULL REFERENCES parents(id));\
                     INSERT INTO parents VALUES (1), (2);\
                     INSERT INTO children VALUES (1, 1);",
                )
                .unwrap();
        }

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        session.commit("base").unwrap();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_new_branch("hosted", None).unwrap();
        drop(repo);
        Connection::open(&database_path)
            .unwrap()
            .execute("DELETE FROM parents WHERE id = 2", [])
            .unwrap();
        session.open().unwrap();
        session.add_all().unwrap();
        let hosted = session.commit("hosted delete").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_branch("main").unwrap();
        drop(repo);
        {
            let database = Connection::open(&database_path).unwrap();
            database
                .execute_batch(
                    "INSERT OR IGNORE INTO parents VALUES (2);\
                     UPDATE children SET parent_id = 2 WHERE id = 1;",
                )
                .unwrap();
        }
        session.open().unwrap();
        session.add_all().unwrap();
        let local = session.commit("local child update").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let plan = session
            .plan_merge(&PlanMergeOptions {
                revision: hosted.clone(),
                expected_head: Some(local.clone()),
            })
            .unwrap();
        let applied = session
            .apply_merge(&ApplyMergeOptions {
                revision: hosted,
                expected_head: Some(local),
                plan_token: plan.plan_token,
            })
            .unwrap();
        let MergeStatus::Merging { unmerged_count, state_token, .. } = applied.merge else {
            panic!("expected validation failure to retain merge state");
        };
        assert_eq!(unmerged_count, 1);
        let conflicts = session
            .list_merge_conflicts(&ListMergeConflictsOptions {
                path: PathBuf::from("constraints.db"),
                limit: 10,
                after: None,
                expected_state_token: state_token.clone(),
            })
            .unwrap();
        assert_eq!(conflicts.items.len(), 1);
        assert_eq!(conflicts.items[0]["kind"], "validation");
        assert_eq!(conflicts.items[0]["reason"], "candidate_validation_failed");
        assert_eq!(
            conflicts.items[0]["recommended_action"],
            "inspect_candidate_constraints"
        );
        assert!(
            conflicts.items[0]["message"]
                .as_str()
                .unwrap()
                .contains("foreign_key_check")
        );
        assert_eq!(
            Connection::open(&database_path)
                .unwrap()
                .query_row("SELECT parent_id FROM children WHERE id = 1", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            2
        );
        assert_eq!(
            Connection::open(&database_path)
                .unwrap()
                .query_row("SELECT COUNT(*) FROM parents WHERE id = 2", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            1
        );

        let index_path = directory.path().join(".graft/index/state.toml");
        let index_before = fs::read(&index_path).unwrap();
        let journal_path = directory
            .path()
            .join(".graft/merge-resolution-session.json");
        let journal_before = fs::read(&journal_path).unwrap();
        let database_before = fs::read(&database_path).unwrap();
        let error = session
            .continue_merge(&ContinueMergeOptions {
                message: "must not commit".to_string(),
                expected_state_token: state_token.clone(),
            })
            .unwrap_err();
        assert_eq!(error.code(), SdkErrorCode::RepositoryCommand);
        assert_eq!(fs::read(&index_path).unwrap(), index_before);
        assert_eq!(fs::read(&journal_path).unwrap(), journal_before);
        assert_eq!(fs::read(&database_path).unwrap(), database_before);
        let MergeStatus::Merging { state_token: unchanged, .. } =
            session.get_merge_status().unwrap()
        else {
            panic!("expected failed retry to preserve merge state");
        };
        assert_eq!(unchanged, state_token);

        session
            .abort_merge(&AbortMergeOptions { expected_state_token: state_token })
            .unwrap();
        session.close().unwrap();
    }

    #[test]
    fn table_view_name_conflicts_can_select_both_directions_without_btree_reads() {
        let directory = tempfile::tempdir().unwrap();
        let hosted_view = directory.path().join("hosted-view.db");
        let hosted_table = directory.path().join("hosted-table.db");
        let rewrite = |path: &Path, object_sql: &str| {
            for candidate in [
                path.to_path_buf(),
                PathBuf::from(format!("{}-wal", path.display())),
                PathBuf::from(format!("{}-shm", path.display())),
            ] {
                let _ = fs::remove_file(candidate);
            }
            let database = Connection::open(path).unwrap();
            database
                .execute_batch(&format!(
                    "CREATE TABLE seed (id INTEGER PRIMARY KEY, value TEXT NOT NULL);\
                     INSERT INTO seed VALUES (1, 'base');\
                     {object_sql}"
                ))
                .unwrap();
        };
        rewrite(&hosted_view, "");
        rewrite(&hosted_table, "");

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        session.commit("base").unwrap();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_new_branch("hosted", None).unwrap();
        drop(repo);
        rewrite(
            &hosted_view,
            "CREATE VIEW shared AS SELECT id, value FROM seed;",
        );
        rewrite(
            &hosted_table,
            "CREATE TABLE shared (id INTEGER PRIMARY KEY, value TEXT NOT NULL); INSERT INTO shared VALUES (1, 'hosted');",
        );
        session.open().unwrap();
        session.add_all().unwrap();
        let hosted = session.commit("hosted objects").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_branch("main").unwrap();
        drop(repo);
        rewrite(
            &hosted_view,
            "CREATE TABLE shared (id INTEGER PRIMARY KEY, value TEXT NOT NULL); INSERT INTO shared VALUES (1, 'local');",
        );
        rewrite(
            &hosted_table,
            "CREATE VIEW shared AS SELECT id, value FROM seed;",
        );
        session.open().unwrap();
        session.add_all().unwrap();
        let local = session.commit("local objects").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let plan = session
            .plan_merge(&PlanMergeOptions {
                revision: hosted.clone(),
                expected_head: Some(local.clone()),
            })
            .unwrap();
        let applied = session
            .apply_merge(&ApplyMergeOptions {
                revision: hosted,
                expected_head: Some(local),
                plan_token: plan.plan_token,
            })
            .unwrap();
        let MergeStatus::Merging { state_token, .. } = applied.merge else {
            panic!("expected object type conflicts");
        };

        let first = session
            .set_merge_path_result(&SetMergePathResultOptions {
                path: PathBuf::from("hosted-view.db"),
                result: MergePathResult::Theirs,
                expected_state_token: state_token,
            })
            .unwrap();
        let MergeStatus::Merging { state_token: second_token, .. } = first.merge else {
            panic!("expected second path conflict");
        };
        let second = session
            .set_merge_path_result(&SetMergePathResultOptions {
                path: PathBuf::from("hosted-table.db"),
                result: MergePathResult::Theirs,
                expected_state_token: second_token,
            })
            .unwrap();
        let MergeStatus::Merging {
            unmerged_count, state_token: final_token, ..
        } = second.merge
        else {
            panic!("expected fully resolved active merge");
        };
        assert_eq!(unmerged_count, 0);
        let completed = session
            .continue_merge(&ContinueMergeOptions {
                message: "select hosted object types".to_string(),
                expected_state_token: final_token,
            })
            .unwrap();
        assert_eq!(completed.merge, MergeStatus::None);
        for (path, expected) in [(&hosted_view, "view"), (&hosted_table, "table")] {
            assert_eq!(
                Connection::open(path)
                    .unwrap()
                    .query_row(
                        "SELECT type FROM sqlite_schema WHERE name = 'shared'",
                        [],
                        |row| row.get::<_, String>(0)
                    )
                    .unwrap(),
                expected
            );
        }
        session.close().unwrap();
    }

    #[test]
    fn compatible_column_table_index_view_and_trigger_additions_form_one_union() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("schema-union.db");
        let rewrite = |branch_sql: &str| {
            for candidate in [
                database_path.clone(),
                PathBuf::from(format!("{}-wal", database_path.display())),
                PathBuf::from(format!("{}-shm", database_path.display())),
            ] {
                let _ = fs::remove_file(candidate);
            }
            Connection::open(&database_path)
                .unwrap()
                .execute_batch(&format!(
                    "CREATE TABLE records (id INTEGER PRIMARY KEY, value TEXT NOT NULL);\
                     CREATE TABLE audit (id INTEGER PRIMARY KEY, source TEXT NOT NULL);\
                     INSERT INTO records VALUES (1, 'base');\
                     {branch_sql}"
                ))
                .unwrap();
        };
        rewrite("");

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        session.commit("base").unwrap();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_new_branch("hosted", None).unwrap();
        drop(repo);
        rewrite(
            "ALTER TABLE records ADD COLUMN hosted_note TEXT;\
             CREATE TABLE hosted_table (id INTEGER PRIMARY KEY);\
             CREATE INDEX hosted_index ON records(value);\
             CREATE VIEW hosted_view AS SELECT id, value FROM records;\
             CREATE TRIGGER hosted_trigger AFTER INSERT ON records BEGIN INSERT INTO audit(source) VALUES ('hosted'); END;",
        );
        session.open().unwrap();
        session.add_all().unwrap();
        let hosted = session.commit("hosted schema").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        session.close().unwrap();

        let repo = Repository::open(directory.path()).unwrap();
        repo.switch_branch("main").unwrap();
        drop(repo);
        rewrite(
            "ALTER TABLE records ADD COLUMN local_note INTEGER;\
             CREATE TABLE local_table (id INTEGER PRIMARY KEY);\
             CREATE INDEX local_index ON records(id, value);\
             CREATE VIEW local_view AS SELECT value FROM records;\
             CREATE TRIGGER local_trigger AFTER DELETE ON records BEGIN INSERT INTO audit(source) VALUES ('local'); END;",
        );
        session.open().unwrap();
        session.add_all().unwrap();
        let local = session.commit("local schema").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let plan = session
            .plan_merge(&PlanMergeOptions {
                revision: hosted.clone(),
                expected_head: Some(local.clone()),
            })
            .unwrap();
        let applied = session
            .apply_merge(&ApplyMergeOptions {
                revision: hosted,
                expected_head: Some(local),
                plan_token: plan.plan_token,
            })
            .unwrap();
        let MergeStatus::Merging { unmerged_count, state_token, .. } = applied.merge else {
            panic!("expected active merge with staged union");
        };
        assert_eq!(unmerged_count, 0);
        let database = Connection::open(&database_path).unwrap();
        let columns = database
            .prepare("SELECT name FROM pragma_table_info('records') ORDER BY cid")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(columns, vec!["id", "value", "local_note", "hosted_note"]);
        let objects = database
            .prepare(
                "SELECT type, name FROM sqlite_schema WHERE name LIKE 'local_%' OR name LIKE 'hosted_%' ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(objects.len(), 8);
        assert!(objects.contains(&("table".to_string(), "hosted_table".to_string())));
        assert!(objects.contains(&("index".to_string(), "hosted_index".to_string())));
        assert!(objects.contains(&("view".to_string(), "hosted_view".to_string())));
        assert!(objects.contains(&("trigger".to_string(), "hosted_trigger".to_string())));
        drop(database);
        let completed = session
            .continue_merge(&ContinueMergeOptions {
                message: "merge compatible schema".to_string(),
                expected_state_token: state_token,
            })
            .unwrap();
        assert_eq!(completed.merge, MergeStatus::None);
        session.close().unwrap();
    }

    #[test]
    fn malformed_tracked_sqlite_is_dirty_diagnostic_and_cannot_be_silently_committed() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("tracked.db");
        Connection::open(&database_path)
            .unwrap()
            .execute("CREATE TABLE docs (id INTEGER PRIMARY KEY)", [])
            .unwrap();
        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        let base = session.commit("base").unwrap()["commit"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        fs::write(&database_path, b"not a sqlite database").unwrap();
        let status = session.status_incremental().unwrap().status;
        assert!(status.dirty);
        assert!(status.has_unstaged_changes);
        assert_eq!(status.unstaged, vec!["tracked.db"]);
        assert_eq!(status.path_diagnostics.len(), 1);
        assert_eq!(status.path_diagnostics[0].path, "tracked.db");
        assert_eq!(
            status.path_diagnostics[0].status,
            graft::repo::RepoPathDiagnosticStatus::Corrupt
        );
        assert_eq!(status.path_diagnostics[0].operation, "sqlite_status");
        assert!(!status.path_diagnostics[0].protected_by_index);

        let add_error = session.add_all().unwrap_err();
        assert_eq!(add_error.code(), SdkErrorCode::RepositoryCommand);
        assert!(add_error.message().contains("sqlite-analysis-failed"));
        let commit_error = session.commit("must not commit").unwrap_err();
        assert_eq!(commit_error.code(), SdkErrorCode::RepositoryCommand);
        assert!(commit_error.message().contains("no-staged-changes"));
        assert_eq!(
            session
                .repository_metadata()
                .unwrap()
                .current_head
                .as_deref(),
            Some(base.as_str())
        );
        assert!(session.status_incremental().unwrap().status.dirty);
        session.close().unwrap();
    }

    #[test]
    fn credentials_are_not_accepted_in_http_remote_urls() {
        let session = RepositorySession::new(".");
        for url in [
            "https://token@example.com/org/repo",
            "https://example.com/org/repo?token=secret",
            "https://example.com/org/repo#secret",
        ] {
            let error = session
                .clone_repository(url, None, Some("secret".to_string()))
                .unwrap_err();
            assert_eq!(error.code(), SdkErrorCode::InvalidArgument);
            assert!(!error.to_string().contains("secret"));
        }
    }

    #[test]
    fn remote_and_branch_arguments_cannot_be_reinterpreted_as_flags() {
        let session = RepositorySession::new(".");
        for remote in ["--force", "-f"] {
            let error = session.push(Some(remote), None).unwrap_err();
            assert_eq!(error.code(), SdkErrorCode::InvalidArgument);
        }
        for branch in ["--force", "-f"] {
            let error = session.push(Some("origin"), Some(branch)).unwrap_err();
            assert_eq!(error.code(), SdkErrorCode::InvalidArgument);
        }
    }
}
