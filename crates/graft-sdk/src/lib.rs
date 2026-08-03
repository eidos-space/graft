//! Long-lived, serialized repository sessions for embedding Graft.
//!
//! This crate is the stable boundary between Graft's repository command implementation and
//! language bindings. It deliberately reuses [`graft_sqlite::repo_service`] rather than
//! reimplementing repository or remote protocols.

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU8, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub use graft::repo::{CancellationToken, RepoPathContent, RepoPathContentState};
use graft::repo::{CommitArtifactState, CommitFileState, RepoStatus, Repository, index::Index};
use graft::{
    core::byte_unit::ByteUnit,
    remote::{RemoteConfig, RemoteCredentialErr, RemoteCredentials},
};
use graft_sqlite::{
    repo_service::{RepositoryCommand, RepositoryCommandService},
    vfs::ErrCtx,
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
}

impl RepositoryOperation {
    /// Whether the operation can replace, create, or remove physical worktree files.
    pub const fn materializes_worktree(self) -> bool {
        matches!(
            self,
            Self::Restore | Self::RestorePaths | Self::Pull | Self::Clone
        )
    }
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
            let config = repo.config().map_err(repo_error)?;
            graft::repo::cancellation_checkpoint().map_err(repo_error)?;
            Ok(RepositoryMetadataResult {
                current_head,
                current_branch,
                upstream,
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
                let mut results = Vec::with_capacity(paths.len());
                for path in paths {
                    graft::repo::cancellation_checkpoint().map_err(repo_error)?;
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
            let diff = self.execute_json("json_diff", Some(&argument))?;
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

fn repository_command_error(error: ErrCtx) -> SdkError {
    if matches!(&error, ErrCtx::Repo(graft::repo::RepoErr::Cancelled)) {
        return SdkError::new(SdkErrorCode::Cancelled, "operation cancelled");
    }
    let message = error.to_string();
    let code = sdk_error_code_for_error(&error, &message);
    SdkError::new(code, message)
}

fn sdk_error_code_for_error(error: &ErrCtx, message: &str) -> SdkErrorCode {
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
        _ => sdk_error_code_for_message(message),
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
        assert!(!RepositoryOperation::Init.materializes_worktree());
        assert!(!RepositoryOperation::Status.materializes_worktree());
        assert!(!RepositoryOperation::StatusIncremental.materializes_worktree());
        assert!(!RepositoryOperation::Diff.materializes_worktree());
        assert!(!RepositoryOperation::DiffPaths.materializes_worktree());
        assert!(!RepositoryOperation::ReadPathContent.materializes_worktree());
        assert!(!RepositoryOperation::AddAll.materializes_worktree());
        assert!(!RepositoryOperation::StagePaths.materializes_worktree());
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
