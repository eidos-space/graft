//! Long-lived, serialized repository sessions for embedding Graft.
//!
//! This crate is the stable boundary between Graft's repository command implementation and
//! language bindings. It deliberately reuses [`graft_sqlite::repo_service`] rather than
//! reimplementing repository or remote protocols.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU8, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use graft::remote::{RemoteCredentialErr, RemoteCredentials};
pub use graft::repo::CancellationToken;
use graft::repo::{CommitArtifactState, CommitFileState, RepoStatus, Repository, index::Index};
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
const MAX_BATCH_MUTATION_PATHS: usize = 1_000;
const MAX_INVENTORY_PAGE_SIZE: usize = 1_000;
const MAX_IGNORE_QUERY_PATHS: usize = 1_000;

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
    History,
    HistorySummaries,
    CommitDetails,
    CommitChangedPaths,
    IsIgnoredPath,
    IsIgnoredPaths,
    Inventory,
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
}

#[derive(Debug, Clone)]
pub struct DiffPathsOptions {
    pub paths: Vec<PathBuf>,
    pub rows: bool,
    pub root: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub limit: usize,
    pub after: Option<String>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffPathsResult {
    pub paths: Vec<PathDiffResult>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub telemetry: DiffTelemetry,
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

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
            let physical = self.target.parent().unwrap_or(&self.target).join(&path);
            if fs::symlink_metadata(&physical).is_ok_and(|metadata| metadata.file_type().is_dir()) {
                return Err(invalid_argument(format!(
                    "diff path `{path}` is a directory; provide explicit changed file paths"
                )));
            }
            let diff = self.diff(&DiffOptions {
                rows: options.rows,
                root: options.root.clone(),
                from: options.from.clone(),
                to: options.to.clone(),
                path: Some(PathBuf::from(&path)),
                ..DiffOptions::default()
            })?;
            results.push(PathDiffResult { path, diff });
        }
        let changed_paths = results
            .iter()
            .filter(|entry| value_changed_path_count(&entry.diff) > 0)
            .count();
        let next_cursor = results.last().map(|entry| entry.path.clone());
        Ok(DiffPathsResult {
            telemetry: DiffTelemetry {
                duration_us: elapsed_us(started),
                requested_paths,
                returned_paths: results.len(),
                changed_paths,
            },
            paths: results,
            has_more,
            next_cursor,
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

        let result = self.with_service(|service| {
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
        });
        if result.is_ok() {
            let _ = self.invalidate_status_cache();
        }
        result
    }

    pub fn push(&self, remote: Option<&str>, branch: Option<&str>) -> Result<Value> {
        let argument = remote_branch_argument(remote, branch)?;
        self.execute_json("json_push", argument.as_deref())
    }

    pub fn fetch(&self, remote: Option<&str>, branch: Option<&str>) -> Result<Value> {
        let argument = remote_branch_argument(remote, branch)?;
        self.execute_json_mutating("json_fetch", argument.as_deref())
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

    fn invalidate_status_cache(&self) -> Result<()> {
        self.with_state(|state| {
            state.status_cache.invalidate();
            Ok(())
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
        let lowercase = message.to_ascii_lowercase();
        let code = if lowercase.contains("locked")
            || lowercase.contains("database lock")
            || lowercase.contains("already held")
        {
            SdkErrorCode::RepositoryBusy
        } else {
            SdkErrorCode::RepositoryCommand
        };
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
    let started = Instant::now();
    let repo = service.repository().map_err(repository_command_error)?;
    let head_target = repo.head_target().map_err(repo_error)?;
    let index = repo.read_index().map_err(repo_error)?;
    let index_changed = !cache.index_metadata_initialized || cache.index != index;
    if index_changed {
        cache.tracked_ignored_paths = None;
    }
    let tree_cache_hit = cache.initialized && cache.head_target == head_target;
    let same_repository_state = cache.initialized && tree_cache_hit && cache.index == index;

    if same_repository_state {
        let tracked = tracked_fingerprints(&repo, &cache.files, &cache.artifacts)?;
        let untracked = visible_untracked_fingerprints(&repo, &cache.files, &cache.artifacts)?;
        let metadata_cache_hits = matching_fingerprint_count(&cache.tracked_fingerprints, &tracked)
            + matching_fingerprint_count(&cache.untracked_fingerprints, &untracked);
        let paths_examined = tracked.len() + untracked.len();
        if tracked == cache.tracked_fingerprints && untracked == cache.untracked_fingerprints {
            let status = cache
                .status
                .clone()
                .expect("initialized status cache contains a status");
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
                },
            ));
        }
    }

    let previous_status = cache.status.clone();
    let status = service.status().map_err(repository_command_error)?;
    if !same_repository_state {
        cache.files = repo.index_files().map_err(repo_error)?;
        cache.artifacts = repo.index_artifacts().map_err(repo_error)?;
    }
    cache.tracked_fingerprints = tracked_fingerprints(&repo, &cache.files, &cache.artifacts)?;
    cache.untracked_fingerprints =
        visible_untracked_fingerprints(&repo, &cache.files, &cache.artifacts)?;
    cache.head_target = head_target;
    cache.index = index;
    cache.index_metadata_initialized = true;
    cache.initialized = true;
    if status_changed(previous_status.as_ref(), &status)? {
        cache.generation = cache.generation.saturating_add(1).max(1);
    }
    cache.status = Some(status.clone());
    let paths_examined = cache.tracked_fingerprints.len() + cache.untracked_fingerprints.len();
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
    IncrementalStatusResult {
        generation: cache.generation,
        change_token: format!("{head}:{}", cache.generation),
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

fn visible_untracked_fingerprints(
    repo: &Repository,
    files: &BTreeMap<String, CommitFileState>,
    artifacts: &BTreeMap<String, CommitArtifactState>,
) -> Result<BTreeMap<String, FileFingerprint>> {
    let mut visible = BTreeMap::new();
    collect_visible_files(repo, repo.worktree(), &mut visible)?;
    visible.retain(|key, _| {
        !files.contains_key(key) && !artifacts.contains_key(key) && !is_sqlite_sidecar_key(key)
    });
    Ok(visible)
}

fn collect_visible_files(
    repo: &Repository,
    directory: &Path,
    visible: &mut BTreeMap<String, FileFingerprint>,
) -> Result<()> {
    if !directory.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(directory).map_err(|error| repository_command_error(error.into()))? {
        graft::repo::cancellation_checkpoint().map_err(repo_error)?;
        let entry = entry.map_err(|error| repository_command_error(error.into()))?;
        let path = entry.path();
        if repo.is_internal_worktree_path(&path) {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| repository_command_error(error.into()))?;
        if repo.is_ignored_worktree_path(&path).map_err(repo_error)? {
            continue;
        }
        if file_type.is_dir() {
            collect_visible_files(repo, &path, visible)?;
        } else if file_type.is_file() {
            let key = repo.file_key(&path).map_err(repo_error)?;
            let fingerprint = fingerprint_path(&path)?
                .expect("directory entry remains present while it is fingerprinted");
            visible.insert(key, fingerprint);
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
    if !directory.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(directory).map_err(|error| repository_command_error(error.into()))? {
        graft::repo::cancellation_checkpoint().map_err(repo_error)?;
        let entry = entry.map_err(|error| repository_command_error(error.into()))?;
        let path = entry.path();
        if repo.is_internal_worktree_path(&path) {
            continue;
        }
        *paths_examined = (*paths_examined).saturating_add(1);
        let file_type = entry
            .file_type()
            .map_err(|error| repository_command_error(error.into()))?;
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
    let lowercase = message.to_ascii_lowercase();
    let code = if lowercase.contains("locked")
        || lowercase.contains("database lock")
        || lowercase.contains("already held")
    {
        SdkErrorCode::RepositoryBusy
    } else {
        SdkErrorCode::RepositoryCommand
    };
    SdkError::new(code, message)
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

    let mut parts = Vec::new();
    if options.rows {
        parts.push("--rows".to_string());
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
        assert!(!RepositoryOperation::RemoteConfigure.materializes_worktree());
        assert!(!RepositoryOperation::Push.materializes_worktree());
        assert!(!RepositoryOperation::Fetch.materializes_worktree());
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
        session.commit("initial").unwrap();
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
            1
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
